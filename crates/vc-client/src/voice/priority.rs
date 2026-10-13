//! Best-effort scheduling for the dedicated pipeline; never call this on a shared worker.

use std::{marker::PhantomData, rc::Rc};

/// Thread-affine so cleanup cannot accidentally restore another thread's policy.
pub(super) struct Priority(platform::Priority, PhantomData<Rc<()>>);

impl Priority {
    pub(super) fn promote() -> Option<Self> {
        match platform::Priority::promote() {
            Ok(priority) => {
                tracing::debug!(frames = super::FRAME, rate = super::io::RATE, "voice pipeline priority elevated");
                Some(Self(priority, PhantomData))
            }
            Err(e) => {
                // One attempt per pipeline, including on hosts without RT permissions.
                tracing::debug!("voice pipeline priority unavailable, continuing normally: {e:#}");
                None
            }
        }
    }
}

impl Drop for Priority {
    fn drop(&mut self) {
        if let Err(e) = self.0.restore() {
            tracing::debug!("voice pipeline priority cleanup failed: {e:#}");
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use anyhow::{Result, ensure};
    use mach2::{
        kern_return::{KERN_SUCCESS, kern_return_t},
        mach_time::{mach_timebase_info, mach_timebase_info_data_t},
        thread_policy::{
            THREAD_EXTENDED_POLICY, THREAD_EXTENDED_POLICY_COUNT, THREAD_TIME_CONSTRAINT_POLICY,
            THREAD_TIME_CONSTRAINT_POLICY_COUNT, thread_extended_policy_data_t, thread_policy_get, thread_policy_set,
            thread_time_constraint_policy_data_t,
        },
    };

    pub(super) struct Priority {
        previous: Option<thread_time_constraint_policy_data_t>,
        extended: thread_extended_policy_data_t,
    }

    fn check(code: kern_return_t) -> Result<()> {
        ensure!(code == KERN_SUCCESS, "Mach thread policy error {code}");
        Ok(())
    }

    fn current_thread() -> u32 {
        // Borrow the pthread's Mach port; unlike mach_thread_self(), this acquires no send right.
        unsafe { libc::pthread_mach_thread_np(libc::pthread_self()) }
    }

    pub(super) fn policy() -> Result<Option<thread_time_constraint_policy_data_t>> {
        let mut policy =
            thread_time_constraint_policy_data_t { period: 0, computation: 0, constraint: 0, preemptible: 0 };
        let mut count = THREAD_TIME_CONSTRAINT_POLICY_COUNT;
        let mut default = 0;
        // SAFETY: the buffer and count match this flavor; the port belongs to this live thread.
        check(unsafe {
            thread_policy_get(
                current_thread(),
                THREAD_TIME_CONSTRAINT_POLICY,
                (&mut policy as *mut thread_time_constraint_policy_data_t).cast(),
                &mut count,
                &mut default,
            )
        })?;
        // A non-RT thread returns default constraints, not its active scheduling policy.
        Ok((default == 0).then_some(policy))
    }

    impl Priority {
        pub(super) fn promote() -> Result<Self> {
            let previous = policy()?;
            let mut extended = thread_extended_policy_data_t { timeshare: 1 };
            let mut count = THREAD_EXTENDED_POLICY_COUNT;
            let mut default = 0;
            // SAFETY: correctly sized policy/timebase outputs on the calling thread.
            check(unsafe {
                thread_policy_get(
                    current_thread(),
                    THREAD_EXTENDED_POLICY,
                    (&mut extended as *mut thread_extended_policy_data_t).cast(),
                    &mut count,
                    &mut default,
                )
            })?;
            let mut timebase = mach_timebase_info_data_t { numer: 0, denom: 0 };
            check(unsafe { mach_timebase_info(&mut timebase) })?;
            ensure!(timebase.numer != 0 && timebase.denom != 0, "invalid Mach timebase");
            let period_ns = super::super::FRAME as u64 * 1_000_000_000 / u64::from(super::super::io::RATE);
            let period = u32::try_from(period_ns * u64::from(timebase.denom) / u64::from(timebase.numer))?;
            // XNU clamps computation to at least half the constraint. Match the 10 ms
            // frame deadline, rather than requesting an arbitrary high fixed priority.
            let mut requested = thread_time_constraint_policy_data_t {
                period,
                computation: period / 2,
                constraint: period,
                preemptible: 1,
            };
            // SAFETY: correctly sized policy input; only this dedicated thread is changed.
            check(unsafe {
                thread_policy_set(
                    current_thread(),
                    THREAD_TIME_CONSTRAINT_POLICY,
                    (&mut requested as *mut thread_time_constraint_policy_data_t).cast(),
                    THREAD_TIME_CONSTRAINT_POLICY_COUNT,
                )
            })?;
            Ok(Self { previous, extended })
        }

        pub(super) fn restore(&mut self) -> Result<()> {
            // Restoring default RT constraints would leave the thread in the RT band.
            // Select its original timeshare/fixed policy when it was not already RT.
            let (flavor, data, count) = if let Some(previous) = self.previous.as_mut() {
                (
                    THREAD_TIME_CONSTRAINT_POLICY,
                    (previous as *mut thread_time_constraint_policy_data_t).cast(),
                    THREAD_TIME_CONSTRAINT_POLICY_COUNT,
                )
            } else {
                (
                    THREAD_EXTENDED_POLICY,
                    (&mut self.extended as *mut thread_extended_policy_data_t).cast(),
                    THREAD_EXTENDED_POLICY_COUNT,
                )
            };
            // SAFETY: flavor, data and count describe the saved policy of this same thread.
            check(unsafe { thread_policy_set(current_thread(), flavor, data, count) })
        }
    }
}

#[cfg(target_os = "windows")]
mod platform {
    use anyhow::{Result, ensure};
    use windows_sys::{
        Win32::{
            Foundation::HANDLE,
            System::Threading::{AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW},
        },
        w,
    };

    pub(super) struct Priority(HANDLE);

    impl Priority {
        pub(super) fn promote() -> Result<Self> {
            let mut index = 0;
            // SAFETY: static NUL-terminated task name and a valid task-index output.
            let handle = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut index) };
            ensure!(!handle.is_null(), "MMCSS Pro Audio registration: {}", std::io::Error::last_os_error());
            Ok(Self(handle))
        }

        pub(super) fn restore(&mut self) -> Result<()> {
            // SAFETY: the successful MMCSS registration is reverted once, on the same thread.
            ensure!(
                unsafe { AvRevertMmThreadCharacteristics(self.0) } != 0,
                "MMCSS cleanup: {}",
                std::io::Error::last_os_error()
            );
            Ok(())
        }
    }
}

// Linux has no OS backstop like XNU demotion or MMCSS reserve: a SCHED_FIFO pipeline
// fed by an always-ready device (ALSA `null`) would hold a core. Stay timeshare there.
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    use anyhow::Result;
    pub(super) struct Priority;
    impl Priority {
        pub(super) fn promote() -> Result<Self> {
            anyhow::bail!("unsupported audio scheduling platform")
        }
        pub(super) fn restore(&mut self) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    #[ignore = "requires permission to set Mach real-time policy"]
    fn macos_policy_changes_and_restores() {
        std::thread::Builder::new()
            .name("vc-voice-pipeline".into())
            .spawn(|| {
                let before = platform::policy().unwrap();
                assert!(before.is_none(), "new pipeline must start with normal scheduling");
                let guard = Priority::promote().expect("Mach promotion refused");
                let during = platform::policy().unwrap().expect("time-constraint policy must be active");
                let mut timebase = mach2::mach_time::mach_timebase_info_data_t { numer: 0, denom: 0 };
                // SAFETY: valid output buffer for the Mach clock conversion.
                assert_eq!(
                    unsafe { mach2::mach_time::mach_timebase_info(&mut timebase) },
                    mach2::kern_return::KERN_SUCCESS
                );
                let period_ns = u64::from(during.period) * u64::from(timebase.numer) / u64::from(timebase.denom);
                assert!((9_999_900..=10_000_100).contains(&period_ns), "frame period: {period_ns} ns");
                assert_eq!(during.computation, during.period / 2);
                assert_eq!(during.constraint, during.period);
                eprintln!("vc-voice-pipeline: before={before:?}, promoted={during:?}, period_ns={period_ns}");
                drop(guard);
                let after = platform::policy().unwrap();
                eprintln!("vc-voice-pipeline: restored={after:?}");
                assert!(after.is_none(), "cleanup must restore normal scheduling");
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
