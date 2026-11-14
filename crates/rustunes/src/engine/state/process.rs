//! Liveness for temporary-state cleanup, without depending on Linux /proc.
pub(crate) fn is_alive(process: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(process) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // Signal zero checks existence only; it sends no signal. A process whose
    // owner prevents inspection must be treated as alive, never safe to prune.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn current_process_is_alive_and_nonprocess_ids_are_not() {
        assert!(is_alive(std::process::id()));
        assert!(!is_alive(0));
        assert!(!is_alive(u32::MAX));
    }
}
