use std::convert::TryInto;
use std::process::{Child, Command};

use anyhow::{bail, Result};
use log::{debug, info, warn};
use nix::{
    sys::signal::{self, Signal},
    unistd::Pid,
};
use procfs::process::{all_processes, Process};

use crate::task_handler::ProcessAction;

pub fn compile_shell_command(command_string: &str) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(command_string);

    command
}

/// Send a signal to one of Pueue's child process handles.
/// We need a special since there exists some inconsistent behavior.
///
/// In some circumstances and environments `sh -c $command` doesn't spawn a shell,
/// but rather spawns the `$command` directly.
///
/// This makes things a lot more complicated, since we need to either send signals
/// to the root process directly OR to all it's child processes.
/// This also affects the `--children` flag on all commands. We then have to either send the signal
/// to all direct children or to all of the childrens' children.
///
/// Returns `Ok(true)`, if everything went alright
/// Returns `Ok(false)`, if the process went away while we tried to send the signal.
pub fn send_signal_to_child(
    child: &Child,
    action: &ProcessAction,
    send_to_children: bool,
) -> Result<bool> {
    let signal = get_signal_from_action(action);
    let pid: i32 = child.id().try_into().unwrap();
    // Check whether this process actually spawned a shell.
    let is_shell = if let Ok(is_shell) = did_process_spawn_shell(pid) {
        is_shell
    } else {
        return Ok(false);
    };

    if is_shell {
        // If it's a shell, we have to send the signal to the actual shell and to all it's children.
        // There might be multiple children, for instance, when users use the `&` operator.
        // If the `send_to_children` flag is given, the

        // Send the signal to the shell, don't propagate to it's children yet.
        send_signal_to_process(pid, action, false)?;

        // Now send the signal to the shells child processes and their respective
        // children if the user wants to do so.
        let shell_children = get_child_processes(pid);
        for shell_child in shell_children {
            send_signal_to_process(shell_child.pid(), action, send_to_children)?;
        }
    } else {
        // If it isn't a shell, send the signal directly to the process.
        // Handle children normally.
        send_signal_to_process(pid, action, send_to_children)?;
    }

    signal::kill(Pid::from_raw(pid), signal)?;
    Ok(true)
}

/// This is a helper function to safely kill a child process.
/// It's purpose is to properly kill all processes and prevent any dangling processes.
///
/// Sadly, this needs some extra handling. Check the docstring of `send_signal_to_child` for
/// additional information on why this has to be done.
///
/// Returns `Ok(true)`, if everything went alright
/// Returns `Ok(false)`, if the process went away while we tried to send the signal.
pub fn kill_child(task_id: usize, child: &mut Child, kill_children: bool) -> bool {
    let pid: i32 = child.id().try_into().unwrap();

    // Check whether this process actually spawned a shell.
    let is_shell = if let Ok(is_shell) = did_process_spawn_shell(pid) {
        is_shell
    } else {
        return false;
    };

    // We have to kill the root process first, to prevent it from spawning new processes.
    // However, this prevents us from getting it's child processes afterwards.
    // That's why we have to get the list of child processes already now.
    let mut child_processes = None;
    if kill_children || is_shell {
        child_processes = Some(get_child_processes(pid));
    }

    // Kill the parent first
    match child.kill() {
        Err(_) => {
            info!("Task {} has already finished by itself", task_id);
            false
        }
        _ => {
            // Now kill all remaining children, after the parent has been killed.
            // If a shell is spawned, we have to manually send the kill signal to all children.
            // Otherwise only send a signal to all children if the `kill_children` flag is set.
            if let Some(child_processes) = child_processes {
                if is_shell {
                    for child_process in child_processes {
                        // Send the signal to each child process, show warning if this fails.
                        let process_pid = child_process.pid();
                        if let Err(error) =
                            send_signal_to_process(process_pid, &ProcessAction::Kill, kill_children)
                        {
                            warn!(
                                "Failed to send kill to pid {} with error {:?}",
                                process_pid, error
                            );
                        }
                    }
                } else if kill_children {
                    send_signal_to_processes(child_processes, &ProcessAction::Kill);
                }
            }

            true
        }
    }
}

/// Check whether a process's commandline string is actually a shell or not
fn did_process_spawn_shell(pid: i32) -> Result<bool> {
    // Get the /proc representation of the child, so we can do some checks
    let process = if let Ok(process) = Process::new(pid) {
        process
    } else {
        info!(
            "Process to kill has probably just gone away. Process {}",
            pid
        );
        bail!("Process has just gone away");
    };

    // Get the root command and, so we check whether it's actually a shell with `sh -c`.
    let mut cmdline = if let Ok(cmdline) = process.cmdline() {
        cmdline
    } else {
        info!(
            "Process to kill has probably just gone away. Process {}",
            pid
        );
        bail!("Process has just gone away");
    };

    if cmdline.len() < 3 {
        return Ok(false);
    }

    if cmdline.remove(0) != "sh" {
        return Ok(false);
    }

    if cmdline.remove(0) != "-c" {
        return Ok(false);
    }

    Ok(true)
}

/// Send a signal to a unix process.
fn send_signal_to_process(
    pid: i32,
    action: &ProcessAction,
    send_to_children: bool,
) -> Result<bool, nix::Error> {
    let signal = get_signal_from_action(action);
    debug!("Sending signal {} to {}", signal, pid);

    // Send the signal to all children, if that's what the user wants.
    if send_to_children {
        send_signal_to_children(pid, action);
    }

    signal::kill(Pid::from_raw(pid), signal)?;
    Ok(true)
}

/// A small helper that sends a signal to all children of a specific process by id.
fn send_signal_to_children(pid: i32, action: &ProcessAction) {
    send_signal_to_processes(get_child_processes(pid), action);
}

/// Send a signal to a list of processes
fn send_signal_to_processes(processes: Vec<Process>, action: &ProcessAction) {
    let signal = get_signal_from_action(action);
    for process in processes {
        // Process is no longer alive, skip this one.
        if !process.is_alive() {
            continue;
        }

        let pid = Pid::from_raw(process.pid);
        if let Err(error) = signal::kill(pid, signal) {
            warn!(
                "Failed send signal {:?} to Pid {}: {:?}",
                signal, process.pid, error
            );
        }
    }
}

/// Get all children of a specific process
fn get_child_processes(pid: i32) -> Vec<Process> {
    let all_processes = match all_processes() {
        Err(error) => {
            warn!("Failed to get full process list: {}", error);
            return Vec::new();
        }
        Ok(processes) => processes,
    };

    all_processes
        .into_iter()
        .filter(|process| process.stat.ppid == pid)
        .collect()
}

fn get_signal_from_action(action: &ProcessAction) -> Signal {
    match action {
        ProcessAction::Kill => Signal::SIGKILL,
        ProcessAction::Pause => Signal::SIGSTOP,
        ProcessAction::Resume => Signal::SIGCONT,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    /// Assert that certain process id no longer exists
    fn process_is_gone(pid: i32) -> bool {
        match Process::new(pid) {
            Ok(process) => !process.is_alive(),
            Err(_) => true,
        }
    }

    #[test]
    fn test_spawn_command() {
        let mut child = compile_shell_command("echo 'this is a test'")
            .spawn()
            .expect("Failed to spawn echo");

        let ecode = child.wait().expect("failed to wait on echo");

        assert!(ecode.success());
    }

    #[test]
    /// Ensure a `sh -c` command will be properly killed without detached processes.
    fn test_shell_command_is_killed() {
        let mut child = compile_shell_command("sleep 60 & sleep 60 && echo 'this is a test'")
            .spawn()
            .expect("Failed to spawn echo");
        let pid: i32 = child.id().try_into().unwrap();
        // Sleep a little to give everything a chance to spawn.
        sleep(Duration::from_millis(500));

        // Make sure the process indeed spawned a shell.
        assert!(did_process_spawn_shell(pid).unwrap());

        // Get all child processes, so we can make sure they no longer exist afterwards.
        let child_processes = get_child_processes(pid);
        assert_eq!(child_processes.len(), 2);

        // Kill the process and make sure it'll be killed.
        assert!(kill_child(0, &mut child, false));

        // Sleep a little to give all processes time to shutdown.
        sleep(Duration::from_millis(500));

        // Assert that the direct child (sh -c) has been killed.
        assert!(process_is_gone(pid));

        // Assert that all child processes have been killed.
        for child_process in child_processes {
            assert!(process_is_gone(child_process.stat.pid));
        }
    }

    #[test]
    /// Ensure that a `sh -c` process with a child process that has children of it's own
    /// will properly kill all processes and their children's children without detached processes.
    fn test_shell_command_children_are_killed() {
        let mut child = compile_shell_command("bash -c 'sleep 60 && sleep 60' && sleep 60")
            .spawn()
            .expect("Failed to spawn echo");
        let pid: i32 = child.id().try_into().unwrap();
        // Sleep a little to give everything a chance to spawn.
        sleep(Duration::from_millis(500));

        // Make sure the process indeed spawned a shell.
        assert!(did_process_spawn_shell(pid).unwrap());

        // Get all child processes and all childrens children,
        // so we can make sure they no longer exist afterwards.
        let child_processes = get_child_processes(pid);
        assert_eq!(child_processes.len(), 1);
        let mut childrens_children = Vec::new();
        for child_process in &child_processes {
            childrens_children.extend(get_child_processes(child_process.stat.pid));
        }
        assert_eq!(childrens_children.len(), 1);

        // Kill the process and make sure its childen will be killed.
        assert!(kill_child(0, &mut child, true));

        // Sleep a little to give all processes time to shutdown.
        sleep(Duration::from_millis(500));

        // Assert that the direct child (sh -c) has been killed.
        assert!(process_is_gone(pid));

        // Assert that all child processes have been killed.
        for child_process in child_processes {
            assert!(process_is_gone(child_process.stat.pid));
        }

        // Assert that all children's child processes have been killed.
        for child_process in childrens_children {
            assert!(process_is_gone(child_process.stat.pid));
        }
    }

    #[test]
    /// Ensure a normal command without `sh -c` will be killed.
    fn test_normal_command_is_killed() {
        let mut child = Command::new("sleep")
            .arg("60")
            .spawn()
            .expect("Failed to spawn echo");
        let pid: i32 = child.id().try_into().unwrap();
        // Sleep a little to give everything a chance to spawn.
        sleep(Duration::from_millis(500));

        // Make sure the process did not spawn a shell.
        assert!(!did_process_spawn_shell(pid).unwrap());

        // No childprocesses exist
        let child_processes = get_child_processes(pid);
        assert_eq!(child_processes.len(), 0);

        // Kill the process and make sure it'll be killed.
        assert!(kill_child(0, &mut child, false));

        // Sleep a little to give all processes time to shutdown.
        sleep(Duration::from_millis(500));

        assert!(process_is_gone(pid));
    }

    #[test]
    /// Ensure a normal command and all it's children will be
    /// properly killed without any detached processes.
    fn test_normal_command_children_are_killed() {
        let mut child = Command::new("bash")
            .arg("-c")
            .arg("sleep 60 & sleep 60 && sleep 60")
            .spawn()
            .expect("Failed to spawn echo");
        let pid: i32 = child.id().try_into().unwrap();
        // Sleep a little to give everything a chance to spawn.
        sleep(Duration::from_millis(500));

        // Make sure the process indeed spawned a shell.
        assert!(!did_process_spawn_shell(pid).unwrap());

        // Get all child processes, so we can make sure they no longer exist afterwards.
        let child_processes = get_child_processes(pid);
        assert_eq!(child_processes.len(), 2);

        // Kill the process and make sure it'll be killed.
        assert!(kill_child(0, &mut child, true));

        // Sleep a little to give all processes time to shutdown.
        sleep(Duration::from_millis(500));

        // Assert that the direct child (sh -c) has been killed.
        assert!(process_is_gone(pid));

        // Assert that all child processes have been killed.
        for child_process in child_processes {
            assert!(process_is_gone(child_process.stat.pid));
        }
    }
}
