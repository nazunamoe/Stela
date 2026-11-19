//! Open file handles taken as directories or regular files by their
//! `FileInfo` attribute (`stela_boot_core::file_kind`), not by uefi-rs's
//! `GetPosition` test, which a FAT driver that answers a directory's
//! `GetPosition` otherwise defeats.
use stela_boot_core::file_kind::{self, Answers, Kind, Probe};
use uefi::{
    Status,
    proto::media::file::{Directory, File as _, FileHandle, FileInfo, RegularFile},
};

struct Handle<'a>(&'a mut FileHandle);

impl Probe for Handle<'_> {
    fn attribute(&mut self) -> Result<u64, Status> {
        self.0
            .get_boxed_info::<FileInfo>()
            .map(|info| info.attribute().bits())
            .map_err(|error| error.status())
    }

    fn position(&mut self) -> Status {
        match self.0.is_regular_file() {
            Ok(true) => Status::SUCCESS,
            Ok(false) => Status::UNSUPPORTED,
            Err(error) => error.status(),
        }
    }
}

/// `handle` as a directory, else what it answered.
pub fn directory(mut handle: FileHandle) -> Result<Directory, Answers> {
    file_kind::expect(&mut Handle(&mut handle), Kind::Directory)?;
    // SAFETY: the handle's FileInfo (or GetPosition) says it is a directory.
    Ok(unsafe { Directory::new(handle) })
}

/// `handle` as a regular file, else what it answered.
pub fn regular(mut handle: FileHandle) -> Result<RegularFile, Answers> {
    file_kind::expect(&mut Handle(&mut handle), Kind::Regular)?;
    // SAFETY: the handle's FileInfo (or GetPosition) says it is a file.
    Ok(unsafe { RegularFile::new(handle) })
}
