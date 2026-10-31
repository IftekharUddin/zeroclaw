//! Windows desktop shutdown over a private inherited stdin pipe. Only ETX
//! requests shutdown; EOF means the desktop chose No or exited unexpectedly.
//! Polling the pipe avoids an uncancellable blocking stdin task at runtime exit.

use std::fs::File;
use std::io::{self, Read};
use std::os::windows::io::{AsRawHandle, FromRawHandle};
use std::time::Duration;

use windows::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, ERROR_BROKEN_PIPE, HANDLE,
};
use windows::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
use windows::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
use windows::Win32::System::Pipes::PeekNamedPipe;
use windows::Win32::System::Threading::GetCurrentProcess;

pub(crate) const STOP: u8 = 0x03;

pub(crate) struct DesktopControl {
    input: Option<File>,
}

impl DesktopControl {
    pub(crate) fn inherited() -> io::Result<Self> {
        // A manually launched supervisor need not have a control pipe.
        let Ok(input) = (unsafe { GetStdHandle(STD_INPUT_HANDLE) }) else {
            return Ok(Self { input: None });
        };
        // SAFETY: GetStdHandle supplies this process's live standard handle.
        if unsafe { GetFileType(input) } != FILE_TYPE_PIPE {
            return Ok(Self { input: None });
        }
        let mut owned = HANDLE::default();
        // SAFETY: duplicate within this process and give File sole ownership
        // of the duplicate. It is not inherited by other child processes.
        unsafe {
            let process = GetCurrentProcess();
            DuplicateHandle(
                process,
                input,
                process,
                &mut owned,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
            .map_err(io::Error::from)?;
            Ok(Self {
                input: Some(File::from_raw_handle(owned.0)),
            })
        }
    }

    pub(crate) fn poll_stop(&mut self) -> io::Result<bool> {
        let Some(input) = self.input.as_mut() else {
            return Ok(false);
        };
        let mut available = 0;
        // SAFETY: input owns the handle; this module is its sole reader, and
        // PeekNamedPipe does not block waiting for input on an anonymous pipe.
        if let Err(error) = unsafe {
            PeekNamedPipe(
                HANDLE(input.as_raw_handle()),
                None,
                0,
                None,
                Some(&mut available),
                None,
            )
        } {
            if error.code() == ERROR_BROKEN_PIPE.to_hresult() {
                self.input = None;
                return Ok(false);
            }
            return Err(io::Error::from(error));
        }
        if available == 0 {
            return Ok(false);
        }
        let mut bytes = [0; 64];
        let length = (available as usize).min(bytes.len());
        let read = input.read(&mut bytes[..length])?;
        if read == 0 {
            self.input = None;
        }
        Ok(bytes[..read].contains(&STOP))
    }

    pub(crate) async fn wait_for_stop(&mut self) -> io::Result<()> {
        loop {
            if self.poll_stop()? {
                return Ok(());
            }
            if self.input.is_none() {
                return std::future::pending().await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
