//! Per-thread COM apartment for ASIO.
//!
//! ASIO drivers are in-process COM servers. The Steinberg host SDK calls
//! `CoInitialize` exactly once, on whichever thread first loads a driver; any
//! later load runs `CoCreateInstance` on the *calling* thread and fails there
//! unless that thread has entered an apartment itself. cpal skips drivers that
//! fail to load, so the device just disappears from enumeration and connect
//! reports "device not found".
//!
//! Every thread that enumerates, opens, or owns an AVB stream therefore holds
//! an [`AudioThreadScope`] for as long as it touches the driver. Scopes nest.
//! On non-Windows platforms this is bookkeeping only.

use std::marker::PhantomData;

pub(super) struct AudioThreadScope {
    #[cfg(windows)]
    com_initialized: bool,
    // COM initialization is per thread; the scope must end on the same thread.
    _not_send: PhantomData<*const ()>,
}

pub(super) fn enter() -> AudioThreadScope {
    #[cfg(test)]
    model::enter();
    AudioThreadScope {
        #[cfg(windows)]
        com_initialized: com::initialize(),
        _not_send: PhantomData,
    }
}

impl Drop for AudioThreadScope {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.com_initialized {
            com::uninitialize();
        }
        #[cfg(test)]
        model::exit();
    }
}

#[cfg(windows)]
mod com {
    use windows_sys::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows_sys::Win32::System::Com::{
        CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
    };

    /// Enters a single-threaded apartment (what the ASIO SDK itself uses).
    /// Returns whether the call must be balanced by [`uninitialize`].
    pub(super) fn initialize() -> bool {
        let hr = unsafe { CoInitializeEx(std::ptr::null(), COINIT_APARTMENTTHREADED as u32) };
        if hr == RPC_E_CHANGED_MODE {
            log::warn!(
                "AVB: thread is already in a multithreaded COM apartment; ASIO drivers may fail to load"
            );
        } else if hr < 0 {
            log::warn!("AVB: CoInitializeEx failed (HRESULT {:#010x})", hr);
        }
        hr >= 0
    }

    pub(super) fn uninitialize() {
        unsafe { CoUninitialize() };
    }
}

/// Test-only mirror of COM apartment state, so the ASIO model in the AVB tests
/// can enforce the same per-thread rules on every platform.
#[cfg(test)]
pub(super) mod model {
    use std::cell::Cell;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Mutex, PoisonError};

    static NEXT_ID: AtomicU64 = AtomicU64::new(1);
    static LIVE: Mutex<Option<HashSet<u64>>> = Mutex::new(None);

    thread_local! {
        static CURRENT: Cell<(usize, u64)> = const { Cell::new((0, 0)) };
    }

    fn live<R>(f: impl FnOnce(&mut HashSet<u64>) -> R) -> R {
        let mut guard = LIVE.lock().unwrap_or_else(PoisonError::into_inner);
        f(guard.get_or_insert_with(HashSet::new))
    }

    pub(crate) fn enter() {
        CURRENT.with(|current| {
            let (depth, id) = current.get();
            if depth == 0 {
                let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
                live(|set| set.insert(id));
                current.set((1, id));
            } else {
                current.set((depth + 1, id));
            }
        });
    }

    pub(crate) fn exit() {
        CURRENT.with(|current| {
            let (depth, id) = current.get();
            if depth == 1 {
                live(|set| set.remove(&id));
                current.set((0, 0));
            } else {
                current.set((depth - 1, id));
            }
        });
    }

    /// The apartment the calling thread is in, if any.
    pub(crate) fn current() -> Option<u64> {
        CURRENT.with(|current| {
            let (depth, id) = current.get();
            (depth > 0).then_some(id)
        })
    }

    pub(crate) fn is_alive(id: u64) -> bool {
        live(|set| set.contains(&id))
    }
}
