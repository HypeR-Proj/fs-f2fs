// SPDX-FileCopyrightText: 2026 roolrz
// SPDX-License-Identifier: Apache-2.0

use super::{Device, Error, FileSystem, INVALID};
const SIZE: usize = 4 * 1024 * 1024;
fn payload() -> Vec<u8> {
    (0..SIZE)
        .map(|i| ((i * 17 + i / 251) % 251) as u8)
        .collect()
}
fn show(e: impl std::fmt::Debug) -> String {
    format!("{e:?}")
}
struct Ram(Vec<u8>);
impl Device for Ram {
    fn blocks(&self) -> u64 {
        (self.0.len() / 4096) as u64
    }
    fn read(&mut self, offset: u64, output: &mut [u8]) -> Result<(), Error> {
        let start = usize::try_from(offset).map_err(|_| INVALID)?;
        let end = start.checked_add(output.len()).ok_or(INVALID)?;
        output.copy_from_slice(self.0.get(start..end).ok_or(INVALID)?);
        Ok(())
    }
    fn write(&mut self, offset: u64, input: &[u8]) -> Result<(), Error> {
        let start = usize::try_from(offset).map_err(|_| INVALID)?;
        let end = start.checked_add(input.len()).ok_or(INVALID)?;
        self.0
            .get_mut(start..end)
            .ok_or(INVALID)?
            .copy_from_slice(input);
        Ok(())
    }
    fn flush(&mut self) -> Result<(), Error> {
        Ok(())
    }
}
#[test]
fn ram_round_trip() -> Result<(), String> {
    let mut device = Ram(vec![0; 128 * 1024 * 1024]);
    FileSystem::format(&mut device, [0x53; 16]).map_err(show)?;
    let mut fs = FileSystem::mount(device).map_err(show)?;
    fs.create("dir", true).map_err(show)?;
    fs.create("dir/data", false).map_err(show)?;
    let bytes = payload();
    for (offset, part) in bytes.chunks(128 * 1024).enumerate() {
        let actual = fs
            .write("dir/data", (offset * 128 * 1024) as u64, part)
            .map_err(show)?;
        if actual != part.len() {
            return Err("short write".into());
        }
    }
    fs.sync().map_err(show)?;
    fs.rename("dir/data", "dir/renamed").map_err(show)?;
    let device = fs.close().map_err(show)?;
    let mut fs = FileSystem::mount(device).map_err(show)?;
    let mut output = vec![0; SIZE];
    if fs.read("dir/renamed", 0, &mut output).map_err(show)? != SIZE || output != bytes {
        return Err("RAM remount mismatch".into());
    }
    fs.resize("dir/renamed", 17).map_err(show)?;
    fs.resize("dir/renamed", 9000).map_err(show)?;
    let actual = fs.read("dir/renamed", 0, &mut output).map_err(show)?;
    if actual != 9000 || output[..17] != bytes[..17] || output[17..9000].iter().any(|b| *b != 0) {
        return Err("RAM truncate mismatch".into());
    }
    fs.close().map_err(show)?;
    Ok(())
}
