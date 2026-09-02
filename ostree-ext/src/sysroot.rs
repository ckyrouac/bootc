//! Helpers for interacting with sysroots.

use std::ops::Deref;

use anyhow::Result;

/// A locked system root.
#[derive(Debug)]
pub struct SysrootLock {
    /// The underlying sysroot value.
    pub sysroot: ostree::Sysroot,
    /// True if we didn't actually lock
    unowned: bool,
}

impl Drop for SysrootLock {
    fn drop(&mut self) {
        if self.unowned {
            return;
        }
        self.sysroot.unlock();
    }
}

impl Deref for SysrootLock {
    type Target = ostree::Sysroot;

    fn deref(&self) -> &Self::Target {
        &self.sysroot
    }
}

impl SysrootLock {
    /// Toggle the finalization lock on a staged deployment.
    #[allow(unsafe_code)]
    pub fn change_finalization(&self, deployment: &ostree::Deployment) -> Result<()> {
        use ostree::glib::translate::*;
        use std::ptr;
        unsafe {
            let mut error = ptr::null_mut();
            let ok = ostree::ffi::ostree_sysroot_change_finalization(
                self.sysroot.to_glib_none().0,
                deployment.to_glib_none().0,
                &mut error,
            );
            if ok == 0 {
                return Err(from_glib_full::<_, ostree::glib::Error>(error).into());
            }
        }
        Ok(())
    }

    /// Asynchronously acquire a sysroot lock.  If the lock cannot be acquired
    /// immediately, a status message will be printed to standard output.
    /// The lock will be unlocked when this object is dropped.
    pub async fn new_from_sysroot(sysroot: &ostree::Sysroot) -> Result<Self> {
        let mut printed = false;
        loop {
            if sysroot.try_lock()? {
                return Ok(Self {
                    sysroot: sysroot.clone(),
                    unowned: false,
                });
            }
            if !printed {
                println!("Waiting for sysroot lock...");
                printed = true;
            }
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
    }

    /// This function should only be used when you have locked the sysroot
    /// externally (e.g. in C/C++ code).  This also does not unlock on drop.
    pub fn from_assumed_locked(sysroot: &ostree::Sysroot) -> Self {
        Self {
            sysroot: sysroot.clone(),
            unowned: true,
        }
    }
}
