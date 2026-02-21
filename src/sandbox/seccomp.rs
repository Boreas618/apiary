//! seccomp filter for network syscall blocking.
//!
//! This module provides seccomp BPF filters to restrict network access
//! and other potentially dangerous syscalls within sandboxes.
//!
//! Note: This module is only functional on Linux. On other platforms,
//! the functions are stubs that return errors.

#[cfg(target_os = "linux")]
use seccompiler::{
    apply_filter, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition,
    SeccompFilter, SeccompRule, TargetArch,
};
#[cfg(target_os = "linux")]
use std::collections::BTreeMap;

use super::SandboxError;
use crate::config::SeccompPolicy;

/// Apply seccomp filter to the current thread.
///
/// This should be called after fork() but before exec() in the child process.
/// Once applied, the filter cannot be removed or relaxed.
#[cfg(target_os = "linux")]
pub fn apply_seccomp_filter(policy: &SeccompPolicy) -> Result<(), SandboxError> {
    let filter = build_filter(policy)?;
    let prog: BpfProgram = filter.try_into().map_err(|e| {
        SandboxError::SeccompFilter(format!("failed to compile seccomp filter: {e}"))
    })?;

    apply_filter(&prog)
        .map_err(|e| SandboxError::SeccompFilter(format!("failed to apply seccomp filter: {e}")))?;

    Ok(())
}

/// Apply seccomp filter to the current thread (non-Linux stub).
#[cfg(not(target_os = "linux"))]
pub fn apply_seccomp_filter(_policy: &SeccompPolicy) -> Result<(), SandboxError> {
    Err(SandboxError::SeccompFilter(
        "seccomp is only available on Linux".to_string(),
    ))
}

/// Build a seccomp filter based on the policy.
#[cfg(target_os = "linux")]
fn build_filter(policy: &SeccompPolicy) -> Result<SeccompFilter, SandboxError> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    if policy.block_network {
        add_network_blocking_rules(&mut rules, policy.allow_unix_sockets)?;
    }

    // Add any additional blocked syscalls
    for syscall_name in &policy.blocked_syscalls {
        if let Some(nr) = syscall_number(syscall_name) {
            rules.insert(nr, vec![]); // Empty vec = unconditional block
        }
    }

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,                     // Default action: allow
        SeccompAction::Errno(libc::EPERM as u32), // Action when rule matches: EPERM
        target_arch(),
    )
    .map_err(|e| SandboxError::SeccompFilter(format!("failed to create filter: {e}")))?;

    Ok(filter)
}

/// Add rules to block network-related syscalls.
#[cfg(target_os = "linux")]
fn add_network_blocking_rules(
    rules: &mut BTreeMap<i64, Vec<SeccompRule>>,
    allow_unix_sockets: bool,
) -> Result<(), SandboxError> {
    // Syscalls to block unconditionally
    let blocked_syscalls = [
        libc::SYS_connect,
        libc::SYS_accept,
        libc::SYS_accept4,
        libc::SYS_bind,
        libc::SYS_listen,
        libc::SYS_getpeername,
        libc::SYS_getsockname,
        libc::SYS_shutdown,
        libc::SYS_sendto,
        libc::SYS_sendmmsg,
        libc::SYS_recvmmsg,
        libc::SYS_getsockopt,
        libc::SYS_setsockopt,
    ];

    for &syscall in &blocked_syscalls {
        rules.insert(syscall, vec![]); // Empty vec = unconditional match
    }

    // For socket() and socketpair(), optionally allow AF_UNIX
    if allow_unix_sockets {
        // Block socket() unless domain == AF_UNIX
        let non_unix_rule = SeccompRule::new(vec![SeccompCondition::new(
            0, // First argument (domain)
            SeccompCmpArgLen::Dword,
            SeccompCmpOp::Ne,
            libc::AF_UNIX as u64,
        )
        .map_err(|e| SandboxError::SeccompFilter(format!("failed to create condition: {e}")))?])
        .map_err(|e| SandboxError::SeccompFilter(format!("failed to create rule: {e}")))?;

        rules.insert(libc::SYS_socket, vec![non_unix_rule.clone()]);
        rules.insert(libc::SYS_socketpair, vec![non_unix_rule]);
    } else {
        // Block all socket creation
        rules.insert(libc::SYS_socket, vec![]);
        rules.insert(libc::SYS_socketpair, vec![]);
    }

    // Also block ptrace to prevent debugging escape
    rules.insert(libc::SYS_ptrace, vec![]);

    Ok(())
}

/// Get the target architecture for seccomp.
#[cfg(target_os = "linux")]
fn target_arch() -> TargetArch {
    #[cfg(target_arch = "x86_64")]
    {
        TargetArch::x86_64
    }
    #[cfg(target_arch = "aarch64")]
    {
        TargetArch::aarch64
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        compile_error!("Unsupported architecture for seccomp")
    }
}

/// Get the syscall number for a syscall name.
#[cfg(target_os = "linux")]
fn syscall_number(name: &str) -> Option<i64> {
    match name.to_lowercase().as_str() {
        "socket" => Some(libc::SYS_socket),
        "socketpair" => Some(libc::SYS_socketpair),
        "connect" => Some(libc::SYS_connect),
        "accept" => Some(libc::SYS_accept),
        "accept4" => Some(libc::SYS_accept4),
        "bind" => Some(libc::SYS_bind),
        "listen" => Some(libc::SYS_listen),
        "sendto" => Some(libc::SYS_sendto),
        "sendmsg" => Some(libc::SYS_sendmsg),
        "sendmmsg" => Some(libc::SYS_sendmmsg),
        "recvfrom" => Some(libc::SYS_recvfrom),
        "recvmsg" => Some(libc::SYS_recvmsg),
        "recvmmsg" => Some(libc::SYS_recvmmsg),
        "shutdown" => Some(libc::SYS_shutdown),
        "getsockname" => Some(libc::SYS_getsockname),
        "getpeername" => Some(libc::SYS_getpeername),
        "getsockopt" => Some(libc::SYS_getsockopt),
        "setsockopt" => Some(libc::SYS_setsockopt),
        "ptrace" => Some(libc::SYS_ptrace),
        "mount" => Some(libc::SYS_mount),
        "umount" => Some(libc::SYS_umount2),
        "pivot_root" => Some(libc::SYS_pivot_root),
        "chroot" => Some(libc::SYS_chroot),
        "setns" => Some(libc::SYS_setns),
        "unshare" => Some(libc::SYS_unshare),
        "clone" => Some(libc::SYS_clone),
        "clone3" => Some(libc::SYS_clone3),
        "reboot" => Some(libc::SYS_reboot),
        "kexec_load" => Some(libc::SYS_kexec_load),
        "init_module" => Some(libc::SYS_init_module),
        "finit_module" => Some(libc::SYS_finit_module),
        "delete_module" => Some(libc::SYS_delete_module),
        _ => None,
    }
}

/// Set PR_SET_NO_NEW_PRIVS to prevent privilege escalation.
///
/// This must be called before applying seccomp filters in non-root context.
#[cfg(target_os = "linux")]
pub fn set_no_new_privs() -> Result<(), SandboxError> {
    let result = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
    if result != 0 {
        return Err(SandboxError::SeccompFilter(format!(
            "failed to set PR_SET_NO_NEW_PRIVS: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// Set PR_SET_NO_NEW_PRIVS (non-Linux stub).
#[cfg(not(target_os = "linux"))]
pub fn set_no_new_privs() -> Result<(), SandboxError> {
    Err(SandboxError::SeccompFilter(
        "PR_SET_NO_NEW_PRIVS is only available on Linux".to_string(),
    ))
}

/// A predefined strict seccomp policy that blocks most dangerous operations.
pub fn strict_policy() -> SeccompPolicy {
    SeccompPolicy {
        block_network: true,
        allow_unix_sockets: true,
        blocked_syscalls: vec![
            "mount".to_string(),
            "umount".to_string(),
            "pivot_root".to_string(),
            "chroot".to_string(),
            "setns".to_string(),
            "unshare".to_string(),
            "reboot".to_string(),
            "kexec_load".to_string(),
            "init_module".to_string(),
            "finit_module".to_string(),
            "delete_module".to_string(),
        ],
        allowed_syscalls: vec![],
    }
}

/// A permissive seccomp policy that only blocks network.
pub fn network_only_policy() -> SeccompPolicy {
    SeccompPolicy {
        block_network: true,
        allow_unix_sockets: true,
        blocked_syscalls: vec![],
        allowed_syscalls: vec![],
    }
}

/// Check if seccomp is available on this system.
#[cfg(target_os = "linux")]
pub fn is_seccomp_available() -> bool {
    // Try to check if seccomp is available by querying the kernel
    let result = unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) };
    // Returns 0 if seccomp is disabled, 2 if in filter mode, -1 with EINVAL if not supported
    result >= 0
}

/// Check if seccomp is available (non-Linux always returns false).
#[cfg(not(target_os = "linux"))]
pub fn is_seccomp_available() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn test_syscall_number() {
        assert_eq!(syscall_number("socket"), Some(libc::SYS_socket));
        assert_eq!(syscall_number("CONNECT"), Some(libc::SYS_connect));
        assert_eq!(syscall_number("unknown"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_target_arch() {
        let arch = target_arch();
        #[cfg(target_arch = "x86_64")]
        assert!(matches!(arch, TargetArch::x86_64));
        #[cfg(target_arch = "aarch64")]
        assert!(matches!(arch, TargetArch::aarch64));
    }

    #[test]
    fn test_strict_policy() {
        let policy = strict_policy();
        assert!(policy.block_network);
        assert!(policy.allow_unix_sockets);
        assert!(!policy.blocked_syscalls.is_empty());
    }

    #[test]
    fn test_network_only_policy() {
        let policy = network_only_policy();
        assert!(policy.block_network);
        assert!(policy.allow_unix_sockets);
        assert!(policy.blocked_syscalls.is_empty());
    }

    #[test]
    fn test_seccomp_available() {
        // Just test that the function doesn't panic
        let _ = is_seccomp_available();
    }
}
