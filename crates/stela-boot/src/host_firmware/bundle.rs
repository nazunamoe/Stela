//! The complete host firmware bundle `stela` puts on the USB's data
//! volume (`stela_loader::host_firmware::bundle`). Before boot services end
//! this reads only the files the PC's PCI functions need, logs each one
//! and the files the bundle lacks, and leaves the bundle as it is. A GPU
//! the firmware left without power answers only after Stela's engine powers
//! its port, when no file can be read through the firmware: where the
//! firmware shows such a port and no GPU of a family Stela drives, that
//! family's files are read too, within the handoff's room
//! (`bundle::Selection::take_late`). A bundle that cannot be read leaves
//! the boot without it, as the log says; the devices that needed its files
//! stay off and their drivers say so.
use super::{open, read_file};
use crate::{firmware, handoff::Region, output, paging::PAGE, pci_io};
use alloc::{format, string::String, vec::Vec};
use stela_boot_core::host_resources::first_bundle;
use stela_loader::host_firmware::{
    MAX_IMAGE_BYTES, MAX_MANIFEST_BYTES,
    bundle::{self, Bundle, Held, Object, Pci, Room},
};
use uefi::{
    boot,
    proto::media::{file::Directory, fs::SimpleFileSystem},
};

/// The files read from the bundle, in the order of their regions.
pub struct Bundled {
    bundle: Bundle<'static>,
    selected: Vec<usize>,
    pub regions: Vec<Region>,
    /// What became of each of the bundle's files (`bundle::held_record`),
    /// for the engine's words about a driver that lacks one.
    pub record: Region,
}

impl Bundled {
    pub fn objects(&self) -> Vec<&Object<'static>> {
        self.selected
            .iter()
            .map(|&index| &self.bundle.objects[index])
            .collect()
    }
}

/// The bundle on the boot volume `root`, else on the first other volume
/// that has one, its files within `room`. A volume whose bundle cannot be
/// read is named and passed over: the USB's data volume, which holds the
/// bundle, may come after another volume whose file system Stela cannot
/// walk.
pub fn read(root: &mut Directory, room: Room) -> Option<Bundled> {
    let boot = core::iter::once_with(|| {
        let place = String::from("boot volume");
        let found = load(root, &place, room);
        (place, found)
    });
    let others = boot::find_handles::<SimpleFileSystem>()
        .ok()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, handle)| {
            // The boot volume itself is held already and refuses this open.
            let mut volume = boot::open_protocol_exclusive::<SimpleFileSystem>(handle).ok()?;
            let mut root = volume.open_volume().ok()?;
            let place = format!("volume {index}");
            let found = load(&mut root, &place, room);
            Some((place, found))
        });
    let (found, unreadable) = first_bundle(boot.chain(others), |place, why| {
        output::arguments(format_args!(
            "HOST FIRMWARE BUNDLE unreadable on {place}, the search goes on: {why}\n"
        ));
    });
    if found.is_none() && unreadable > 0 {
        output::arguments(format_args!(
            "HOST FIRMWARE BUNDLE unreadable, the boot goes on without it: no volume holds a \
             readable one, and {unreadable} could not be searched or held one Stela cannot \
             read, as named above\n"
        ));
    }
    found
}

/// The PCI functions the firmware's drivers found, `None` without its
/// census.
fn functions() -> Option<Vec<Pci>> {
    match pci_io::capture() {
        Ok(census) => Some(
            census
                .functions
                .iter()
                .filter_map(|function| {
                    Pci::from_header(
                        function.address.segment,
                        function.address.bus,
                        &function.header,
                    )
                })
                .collect(),
        ),
        Err(error) => {
            output::arguments(format_args!(
                "HOST FIRMWARE BUNDLE no PCI census ({error:?}): the USB files and every GPU's\n"
            ));
            None
        }
    }
}

/// The bundle in `root`, if it has one.
fn load(root: &mut Directory, place: &str, room: Room) -> Option<Result<Bundled, String>> {
    let file = match open(root, bundle::MANIFEST) {
        Ok(Some(file)) => file,
        Ok(None) => return None,
        Err(error) => return Some(Err(format!("{error}"))),
    };
    Some(load_from(root, file, place, room))
}

fn load_from(
    root: &mut Directory,
    file: uefi::proto::media::file::RegularFile,
    place: &str,
    room: Room,
) -> Result<Bundled, String> {
    let started = output::now();
    let region = read_file(file, None, MAX_MANIFEST_BYTES).map_err(|error| format!("{error}"))?;
    // SAFETY: read_file reserved these pages and completed all writes; they
    // are never freed or written again.
    let bytes: &'static [u8] =
        unsafe { core::slice::from_raw_parts(region.physical as *const u8, region.bytes as usize) };
    let parsed = Bundle::parse(bytes).map_err(|error| error.0)?;
    let functions = functions();
    let mut selection = match &functions {
        Some(functions) => bundle::select(functions),
        None => bundle::select_without_census(bundle::Ports::BUILT),
    };
    for late in &selection.late {
        output::arguments(format_args!(
            "HOST FIRMWARE BUNDLE LATE {}: {} files, {} bytes, for {}\n",
            late.family,
            late.files.len(),
            late.bytes(),
            late.why
        ));
    }
    let left = selection.take_late(room);
    for left in &left {
        output::arguments(format_args!(
            "HOST FIRMWARE BUNDLE LATE {} not read: its {} files of {} bytes do not fit beside \
             the files above in the firmware handoff's {} files and {} bytes; a GPU that \
             answers late lacks them\n",
            left.family,
            left.files.len(),
            left.bytes(),
            room.images,
            room.bytes
        ));
    }
    for why in &selection.unknown {
        output::arguments(format_args!(
            "HOST FIRMWARE BUNDLE NO RECIPE for the NVIDIA display device at {why}: its \
             firmware is not in Stela's catalog\n"
        ));
    }
    let mut bundled = Bundled {
        selected: Vec::new(),
        regions: Vec::new(),
        bundle: parsed,
        record: Region::default(),
    };
    let mut lacking = 0;
    let mut failed: Vec<&str> = Vec::new();
    for (artifact, why) in &selection.wanted {
        let Some(index) = bundled
            .bundle
            .objects
            .iter()
            .position(|object| object.sha256 == artifact.sha256)
        else {
            lacking += 1;
            output::arguments(format_args!(
                "HOST FIRMWARE BUNDLE LACKS {} for {why}: `stela repair` adds it\n",
                artifact.member
            ));
            continue;
        };
        let object = &bundled.bundle.objects[index];
        let path = format!("{}/{}", bundle::DIRECTORY, object.path);
        let read = match open(root, &path) {
            Ok(Some(file)) => read_file(file, Some(object.length), MAX_IMAGE_BYTES),
            Ok(None) => Err(stela_contracts::refuse!("the bundle names a missing file")),
            Err(error) => Err(error),
        };
        match read {
            Ok(region) => {
                output::arguments(format_args!(
                    "HOST FIRMWARE BUNDLE USE {} bytes={} for {why}\n",
                    object.member, object.length
                ));
                bundled.selected.push(index);
                bundled.regions.push(region);
            }
            Err(error) => {
                lacking += 1;
                failed.push(artifact.sha256);
                output::arguments(format_args!(
                    "HOST FIRMWARE BUNDLE LACKS {} for {why}: {error}\n",
                    object.member
                ));
            }
        }
    }
    let record = {
        let bundled = &bundled;
        bundle::held_record(&bundled.bundle, |sha256| {
            if bundled
                .objects()
                .iter()
                .any(|object| object.sha256 == sha256)
            {
                Held::Read
            } else if failed.contains(&sha256) {
                Held::Failed
            } else if left
                .iter()
                .any(|late| late.files.iter().any(|file| file.sha256 == sha256))
            {
                Held::NoRoom
            } else {
                Held::Unread
            }
        })
    };
    bundled.record = keep(&record)?;
    let read_bytes: usize = bundled.objects().iter().map(|object| object.length).sum();
    output::arguments(format_args!(
        "HOST FIRMWARE BUNDLE {place} files={} pci={} used={} lacking={lacking} bytes={read_bytes} \
         read_ms={}\n",
        bundled.bundle.objects.len(),
        functions.as_ref().map_or(0, Vec::len),
        bundled.selected.len(),
        output::millis_since(started)
    ));
    Ok(bundled)
}

/// `bytes` in fresh reserved pages the engine reads after the exit; an
/// empty region for none.
fn keep(bytes: &[u8]) -> Result<Region, String> {
    if bytes.is_empty() {
        return Ok(Region::default());
    }
    let physical =
        firmware::pages(bytes.len().div_ceil(PAGE)).map_err(|error| format!("{error}"))?;
    // SAFETY: fresh reserved LOADER_DATA pages, written once here before
    // any immutable view is published; never freed or given to a guest.
    unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), physical as *mut u8, bytes.len()) };
    Ok(Region {
        physical: physical as u64,
        bytes: bytes.len() as u64,
    })
}
