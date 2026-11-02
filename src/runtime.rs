// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0
use hyper_filesystem::{
    Timestamp,
    protocol::{self, DirectoryCursor, Entry, Error},
};
use hyper_fs_f2fs::{FileSystem, native::BlockDevice};
use hyper_fs_service::Volume;
use hyper_os::block::{BlockQueue, NativeBlock, QueueConfig};
struct Mounted(FileSystem<BlockDevice>);
impl Volume for Mounted {
    fn stat(&mut self, path: &str) -> Result<Entry, Error> {
        let metadata = self.0.stat(path).map_err(map_error)?;
        if !matches!(metadata.mode & 0o170000, 0o040000 | 0o100000) {
            return Err(Error::Unsupported);
        }
        let name = path.rsplit('/').next().ok_or(Error::InvalidInput)?;
        let mut entry = Entry::empty();
        if name.len() > entry.name.len() {
            return Err(Error::InvalidInput);
        }
        entry.name[..name.len()].copy_from_slice(name.as_bytes());
        entry.name_len = name.len();
        entry.kind = if metadata.directory() {
            protocol::DIRECTORY
        } else {
            protocol::FILE
        };
        entry.mode = metadata.mode & 0o777;
        entry.size = metadata.size;
        let time =
            |ns: u64| Timestamp::new((ns / 1_000_000_000) as i64, (ns % 1_000_000_000) as u32);
        entry.created = time(metadata.created);
        entry.accessed = time(metadata.accessed);
        entry.modified = time(metadata.modified);
        Ok(entry)
    }
    fn read_directory(
        &mut self,
        path: &str,
        cursor: DirectoryCursor,
        output: &mut [u8],
    ) -> Result<usize, Error> {
        if cursor.words[2..] != [0, 0] {
            return Err(Error::InvalidInput);
        }
        let mut batch =
            protocol::DirectoryBatchBuilder::new(output, cursor).ok_or(Error::InvalidInput)?;
        let mut position = cursor.words[0];
        let mut skip = cursor.words[1];
        let mut raw = [0; 4096];
        loop {
            let before = position;
            let length = self
                .0
                .read_directory(path, &mut position, &mut raw)
                .map_err(map_error)?;
            if length == 0 {
                return batch.finish(true).ok_or(Error::Corrupt);
            }
            let mut offset = 0;
            let mut index = 0;
            while offset < length {
                let header = raw
                    .get(offset..offset + 10)
                    .filter(|_| offset + 10 <= length)
                    .ok_or(Error::Corrupt)?;
                let size = usize::from(header[8]);
                let end = offset + 10 + size;
                let name = raw
                    .get(offset + 10..end)
                    .filter(|_| end <= length)
                    .ok_or(Error::Corrupt)?;
                offset = end;
                index += 1;
                if index <= skip || name == b"." || name == b".." {
                    continue;
                }
                let name = std::str::from_utf8(name).map_err(|_| Error::Unsupported)?;
                let child = if path.is_empty() {
                    name.to_string()
                } else {
                    format!("{path}/{name}")
                };
                let next = DirectoryCursor {
                    words: if offset == length {
                        [position, 0, 0, 0]
                    } else {
                        [before, index, 0, 0]
                    },
                };
                if !batch
                    .push(&self.stat(&child)?, next)
                    .ok_or(Error::Corrupt)?
                {
                    return batch.finish(false).ok_or(Error::Corrupt);
                }
            }
            if position == before {
                return Err(Error::Corrupt);
            }
            skip = 0;
        }
    }
    fn read(&mut self, p: &str, o: u64, b: &mut [u8]) -> Result<usize, Error> {
        self.0.read(p, o, b).map_err(map_error)
    }
    fn write(&mut self, p: &str, o: u64, b: &[u8]) -> Result<usize, Error> {
        self.0.write(p, o, b).map_err(map_error)
    }
    fn create(&mut self, p: &str, d: bool) -> Result<(), Error> {
        self.0.create(p, d).map_err(map_error)
    }
    fn remove(&mut self, p: &str) -> Result<(), Error> {
        self.0.remove(p).map_err(map_error)
    }
    fn rename(&mut self, p: &str, t: &str) -> Result<(), Error> {
        self.0.rename(p, t).map_err(map_error)
    }
    fn resize(&mut self, p: &str, n: u64) -> Result<(), Error> {
        self.0.resize(p, n).map_err(map_error)
    }
    fn sync(&mut self) -> Result<(), Error> {
        self.0.sync().map_err(map_error)
    }
    fn set_times(
        &mut self,
        p: &str,
        a: Option<Timestamp>,
        m: Option<Timestamp>,
    ) -> Result<(), Error> {
        self.0
            .set_times(p, [a, m].map(|t| t.map(|t| (t.seconds(), t.nanoseconds()))))
            .map_err(map_error)
    }
}
fn map_error(e: hyper_fs_f2fs::Error) -> Error {
    match e.0 {
        -2 => Error::Unsupported,
        -4 => Error::Allocation,
        -10 | -14 | -51 | -52 => Error::InvalidInput,
        -25 => Error::Missing,
        -26 => Error::Exists,
        -24 => Error::Closed,
        -30 => Error::ReadOnly,
        -42 => Error::Corrupt,
        -54 => Error::NoSpace,
        -55 => Error::NotEmpty,
        _ => Error::Io,
    }
}
fn show(e: impl std::fmt::Debug) -> String {
    format!("{e:?}")
}
pub(super) fn run() -> Result<(), String> {
    let mut startup = hyper_rt::process::startup().map_err(show)?;
    let sectors = std::env::args()
        .nth(1)
        .ok_or("missing volume geometry")?
        .parse::<u64>()
        .map_err(show)?;
    if std::env::args().nth(2).as_deref() != Some("rw") {
        return Err("experimental F2FS requires rw; read-only recovery is not implemented".into());
    }
    let (block, adopted) = if std::env::args().nth(3).as_deref() == Some("--adopt") {
        let (b, s) = hyper_fs_service::adopt(&mut startup)?;
        (b, Some(s))
    } else {
        (
            NativeBlock::from_handle(
                startup
                    .take(hyper_service::filesystem::BLOCK)
                    .map_err(show)?,
            ),
            None,
        )
    };
    let queue = match adopted.as_ref() {
        Some(state) => BlockQueue::from_handle(
            state
                .queue()
                .ok_or("missing adopted queue")?
                .duplicate(hyper_service::filesystem::QUEUE_RIGHTS)
                .map_err(show)?,
        ),
        None => block.create_queue(QueueConfig::default()).map_err(show)?,
    };
    let device = BlockDevice::with_queue(
        block,
        sectors,
        BlockQueue::from_handle(
            queue
                .as_handle_ref()
                .duplicate(hyper_service::filesystem::QUEUE_RIGHTS)
                .map_err(show)?,
        ),
    )
    .map_err(show)?;
    let mut volume = Mounted(FileSystem::mount(device).map_err(show)?);
    let completion = match adopted {
        Some(state) => hyper_fs_service::resume(state, &mut volume)?,
        None => hyper_fs_service::serve(&mut startup, &mut volume, queue)?,
    };
    let device = volume.0.close().map_err(show)?;
    #[cfg(feature = "io-statistics")]
    println!("F2FS-IO: {:?}", device.statistics());
    hyper_fs_service::return_block(completion, device.into_block().map_err(show)?)
}
