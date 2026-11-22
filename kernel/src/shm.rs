//! Bounded shared-memory regions for ring-3 processes.
//!
//! A region is a set of physical frames with a kernel-side owner. Processes map
//! a region into a reserved slice of their own address space with `shmattach`;
//! every mapping adds a frame reference, so a region lives until its owner
//! destroys it and every attached process detaches or exits. These mappings are
//! kept separate from ordinary user pages, so `fork` never turns a shared
//! region into a copy-on-write page.
//!
//! Regions are deliberately fixed-size and page-aligned. The syscall surface is
//! intentionally small: `shmcreate`, `shmattach`, `shmdetach`, `shmdestroy` and
//! `shmbind`. The last call lets a scheme server attach retained frames to a
//! stream descriptor, so bulk messages travel through shared memory instead of
//! the bounded pipe buffer.

use alloc::vec::Vec;
use spin::Mutex;
use x86_64::structures::paging::PhysFrame;

use crate::address_space;
use crate::process::{Pid, with_process_mut};

const PAGE_BYTES: u64 = 4096;
/// Maximum number of simultaneously live regions.
const MAX_REGIONS: usize = 16;
/// One region occupies this much of the user virtual address space.
const SHM_SLOT_BYTES: u64 = 0x0008_0000;

struct Region {
    id: u64,
    owner: u64,
    slot: usize,
    frames: Vec<PhysFrame>,
    /// Number of live page mappings across all attached processes.
    mappings: usize,
    attached: Vec<u64>,
    destroyed: bool,
}

struct Table {
    regions: Vec<Region>,
    next_id: u64,
}

static TABLE: Mutex<Table> = Mutex::new(Table {
    regions: Vec::new(),
    next_id: 1,
});

/// Base virtual address of a region slot.
fn region_base(slot: usize) -> u64 {
    crate::usermode::USER_BRK_END + slot as u64 * SHM_SLOT_BYTES
}

/// Largest accepted region length, exposed for documentation and tests.
#[must_use]
pub const fn max_region_bytes() -> u64 {
    SHM_SLOT_BYTES
}

/// A retained kernel view of a shared region's frames.
///
/// The compositor can keep this handle as a window's backing store. Each frame
/// reference is held until the handle is dropped, so the pixels survive even if
/// the region owner is destroyed and every process detaches.
#[derive(Debug)]
pub struct Mapping {
    frames: Vec<PhysFrame>,
    addresses: Vec<u64>,
}

impl Mapping {
    /// Total number of readable bytes in this mapping.
    #[must_use]
    pub fn len(&self) -> usize {
        self.frames.len() * PAGE_BYTES as usize
    }

    /// Whether the mapping has no frames.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Copies bytes out of the shared frames into `destination`.
    pub fn read(&self, offset: usize, destination: &mut [u8]) -> bool {
        if offset
            .checked_add(destination.len())
            .is_none_or(|end| end > self.len())
        {
            return false;
        }
        let mut position = offset;
        let mut copied = 0;
        while copied < destination.len() {
            let index = position / PAGE_BYTES as usize;
            let within = position % PAGE_BYTES as usize;
            let Some(&base) = self.addresses.get(index) else {
                return false;
            };
            let count = (destination.len() - copied).min(PAGE_BYTES as usize - within);
            // SAFETY: `base` is a physical-memory-window address for a frame
            // whose reference this mapping owns, and `count` stays within that
            // frame and the caller's destination slice.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    (base + within as u64) as *const u8,
                    destination[copied..].as_mut_ptr(),
                    count,
                );
            }
            position += count;
            copied += count;
        }
        true
    }

    /// Copies bytes from `source` into the shared frames.
    pub fn write(&self, offset: usize, source: &[u8]) -> bool {
        if offset
            .checked_add(source.len())
            .is_none_or(|end| end > self.len())
        {
            return false;
        }
        let mut position = offset;
        let mut copied = 0;
        while copied < source.len() {
            let index = position / PAGE_BYTES as usize;
            let within = position % PAGE_BYTES as usize;
            let Some(&base) = self.addresses.get(index) else {
                return false;
            };
            let count = (source.len() - copied).min(PAGE_BYTES as usize - within);
            // SAFETY: `base` is a physical-memory-window address for a frame
            // whose reference this mapping owns, and `count` stays within that
            // frame and the caller's source slice.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    source[copied..].as_ptr(),
                    (base + within as u64) as *mut u8,
                    count,
                );
            }
            position += count;
            copied += count;
        }
        true
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        for &frame in &self.frames {
            address_space::release_shared_owner(frame);
        }
    }
}

/// Retains a kernel view of `region_id` when it has at least `needed` bytes.
fn retained_mapping(region_id: u64, needed: usize) -> Option<Mapping> {
    let table = TABLE.lock();
    let region = table
        .regions
        .iter()
        .find(|region| region.id == region_id && !region.destroyed)?;
    if region.frames.is_empty() || region.frames.len() * (PAGE_BYTES as usize) < needed {
        return None;
    }
    let mut frames = Vec::with_capacity(region.frames.len());
    let mut addresses = Vec::with_capacity(region.frames.len());
    for &frame in &region.frames {
        address_space::retain_shared_frame(frame);
        frames.push(frame);
        addresses.push(address_space::frame_window_address(frame));
    }
    Some(Mapping { frames, addresses })
}

/// Retains a kernel view of `region_id` for use as a window backing store.
pub fn window_mapping(region_id: u64, needed: usize) -> Option<Mapping> {
    retained_mapping(region_id, needed)
}

/// Retains a kernel view of `region_id` for use as a scheme stream buffer.
///
/// The returned mapping keeps its frame references until the stream descriptor
/// is closed, even if the region is destroyed or its owner exits.
pub fn stream_mapping(region_id: u64) -> Option<Mapping> {
    retained_mapping(region_id, PAGE_BYTES as usize)
}

impl Table {
    fn free_slot(&self) -> Option<usize> {
        (0..MAX_REGIONS).find(|slot| !self.regions.iter().any(|region| region.slot == *slot))
    }
}

/// Allocates a region owned by `owner` and returns its identifier.
///
/// The region is not mapped yet; each process attaches it explicitly.
pub fn create(owner: u64, length: u64) -> Option<u64> {
    if length == 0 || length > SHM_SLOT_BYTES {
        return None;
    }
    let pages = usize::try_from(length.div_ceil(PAGE_BYTES)).ok()?;
    let mut frames = Vec::with_capacity(pages);
    for _ in 0..pages {
        let Some(frame) = address_space::allocate_frame() else {
            for frame in frames {
                address_space::release_shared_owner(frame);
            }
            return None;
        };
        frames.push(frame);
    }

    let mut table = TABLE.lock();
    let Some(slot) = table.free_slot() else {
        drop(table);
        for frame in frames {
            address_space::release_shared_owner(frame);
        }
        return None;
    };
    let id = table.next_id;
    let Some(next_id) = table.next_id.checked_add(1).filter(|_| id != 0) else {
        drop(table);
        for frame in frames {
            address_space::release_shared_owner(frame);
        }
        return None;
    };
    table.next_id = next_id;
    table.regions.push(Region {
        id,
        owner,
        slot,
        frames,
        mappings: 0,
        attached: Vec::new(),
        destroyed: false,
    });
    Some(id)
}

/// Maps `id` into `pid`'s address space and returns the base virtual address.
pub fn attach(pid: Pid, id: u64) -> Option<u64> {
    // Snapshot the frames first. Mapping below needs the process-table lock, and
    // exit paths already take that lock before calling into this module, so the
    // SHM lock must not be held across `with_process_mut`.
    let (base, frames) = {
        let table = TABLE.lock();
        let region = table
            .regions
            .iter()
            .find(|region| region.id == id && !region.destroyed)?;
        (region_base(region.slot), region.frames.clone())
    };

    let mapped = with_process_mut(pid, |process| {
        process
            .address_space_mut()
            .map_shared(pid.as_u64(), id, base, &frames)
    });
    if !matches!(mapped, Some(Ok(()))) {
        return None;
    }

    // Commit the bookkeeping only after the mapping exists. If the region was
    // retired while the process lock was held, unmap again so its final frame
    // reference is released.
    let committed = {
        let mut table = TABLE.lock();
        table
            .regions
            .iter_mut()
            .find(|region| region.id == id)
            .is_some_and(|region| {
                if region.destroyed || region.attached.contains(&pid.as_u64()) {
                    return false;
                }
                region.attached.push(pid.as_u64());
                region.mappings += frames.len();
                true
            })
    };
    if committed {
        return Some(base);
    }

    let _ = with_process_mut(pid, |process| process.address_space_mut().unmap_shared(id));
    None
}

/// Unmaps `id` from `pid`'s address space.
pub fn detach(pid: Pid, id: u64) -> bool {
    let count =
        with_process_mut(pid, |process| process.address_space_mut().unmap_shared(id)).unwrap_or(0);
    if count == 0 {
        return false;
    }
    let mut table = TABLE.lock();
    if let Some(region) = table.regions.iter_mut().find(|region| region.id == id) {
        region.attached.retain(|&attached| attached != pid.as_u64());
    }
    true
}

/// Retires a region. Its frames stay alive until every mapping is gone.
pub fn destroy(owner: u64, id: u64) -> bool {
    let mut table = TABLE.lock();
    let Some(index) = table
        .regions
        .iter()
        .position(|region| region.id == id && region.owner == owner && !region.destroyed)
    else {
        return false;
    };
    let remove_now = {
        let region = &mut table.regions[index];
        region.destroyed = true;
        for frame in region.frames.drain(..) {
            address_space::release_shared_owner(frame);
        }
        region.mappings == 0
    };
    if remove_now {
        table.regions.remove(index);
    }
    true
}

/// Records that one process mapping disappeared.
///
/// The caller has already released the mapping's frame reference; this only
/// updates the region lifetime and removes destroyed regions.
pub fn on_mapping_released(id: u64, pid: u64) {
    let mut table = TABLE.lock();
    let Some(index) = table.regions.iter().position(|region| region.id == id) else {
        return;
    };
    let remove = {
        let region = &mut table.regions[index];
        region.attached.retain(|&attached| attached != pid);
        region.mappings = region.mappings.saturating_sub(1);
        region.destroyed && region.mappings == 0
    };
    if remove {
        table.regions.remove(index);
    }
}
