//! seccomp filter for network syscall blocking.
//!
//! This module provides seccomp BPF filters to restrict network access
//! and other potentially dangerous syscalls within sandboxes.

use seccompiler::{
    apply_filter, BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition,
    SeccompFilter, SeccompRule, TargetArch,
};
use std::collections::BTreeMap;

use super::SandboxError;
use crate::config::SeccompPolicy;

/// Apply seccomp filter to the current thread.
///
/// This should be called after fork() but before exec() in the child process.
/// Once applied, the filter cannot be removed or relaxed.
pub fn apply_seccomp_filter(policy: &SeccompPolicy) -> Result<(), SandboxError> {
    let filter = build_filter(policy)?;
    let prog: BpfProgram = filter.try_into().map_err(|e| {
        SandboxError::SeccompFilter(format!("failed to compile seccomp filter: {e}"))
    })?;

    apply_filter(&prog)
        .map_err(|e| SandboxError::SeccompFilter(format!("failed to apply seccomp filter: {e}")))?;

    Ok(())
}

fn build_filter(policy: &SeccompPolicy) -> Result<SeccompFilter, SandboxError> {
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();

    if policy.block_network {
        add_network_blocking_rules(&mut rules, policy.allow_unix_sockets)?;
    }

    for syscall_name in &policy.blocked_syscalls {
        if let Some(nr) = syscall_number(syscall_name) {
            rules.insert(nr, vec![]);
        }
    }

    for syscall_name in &policy.allowed_syscalls {
        if let Some(nr) = syscall_number(syscall_name) {
            rules.remove(&nr);
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

fn add_network_blocking_rules(
    rules: &mut BTreeMap<i64, Vec<SeccompRule>>,
    allow_unix_sockets: bool,
) -> Result<(), SandboxError> {
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
        libc::SYS_sendmsg,
        libc::SYS_sendmmsg,
        libc::SYS_recvfrom,
        libc::SYS_recvmsg,
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
pub fn is_seccomp_available() -> bool {
    // Try to check if seccomp is available by querying the kernel
    let result = unsafe { libc::prctl(libc::PR_GET_SECCOMP, 0, 0, 0, 0) };
    // Returns 0 if seccomp is disabled, 2 if in filter mode, -1 with EINVAL if not supported
    result >= 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_syscall_number() {
        assert_eq!(syscall_number("socket"), Some(libc::SYS_socket));
        assert_eq!(syscall_number("CONNECT"), Some(libc::SYS_connect));
        assert_eq!(syscall_number("unknown"), None);
    }

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
        let _ = is_seccomp_available();
    }
}
