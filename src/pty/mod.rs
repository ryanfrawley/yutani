extern crate libc;
use nix::libc::*;
use std::ffi::CString;
use std::ptr::null_mut;

pub fn fork_pty<OnOutput>(fdm: i32, on_output: OnOutput) -> Result<i32, String>
where
    OnOutput: Fn(&[u8]),
{
    let fds: i32;
    let mut rc: i32;
    let mut input: [u8; 1500] = [0; 1500];

    unsafe {
        rc = grantpt(fdm);
        if rc != 0 {
            return Err("Error on grantpt()".to_string());
        }

        rc = unlockpt(fdm);
        if rc != 0 {
            return Err("Error on unlockpt()".to_string());
        }

        // Open the slave pty
        fds = open(ptsname(fdm), O_RDWR);
        println!("Virtual interface configured");

        match nix::unistd::fork() {
            Ok(nix::unistd::ForkResult::Child) => {
                // Close the master side of the pty
                close(fdm);

                // Get the default terminal settings
                let mut slave_settings_orig = std::mem::MaybeUninit::<termios>::uninit();
                tcgetattr(fds, slave_settings_orig.as_mut_ptr());
                let slave_settings_orig = slave_settings_orig.assume_init();

                // Set raw mode on the slave side of the pty
                let mut slave_settings_new = slave_settings_orig.clone();
                slave_settings_new.c_lflag &= !(ECHO | ICANON);
                tcsetattr(fds, TCSANOW, &mut slave_settings_new);

                // The slave side of the PTY becomes the standard input and outputs of the child process
                dup2(fds, 0);
                dup2(fds, 1);
                dup2(fds, 2);

                setsid();

                // As the child is a session leader, set the controlling terminal to be the slave side of the PTY
                // (Mandatory for programs like the shell to make them manage correctly their outputs)
                ioctl(0, TIOCSCTTY.into(), 1);

                let default_shell = CString::new(std::env::var("SHELL").unwrap()).unwrap();

                println!("executing default shell\n");

                match nix::unistd::execvp(default_shell.as_c_str(), &[&default_shell]) {
                    Ok(_) => Ok(0),
                    Err(n) => Err(format!("Failed in execvp(): {n}")),
                }
            }
            Ok(nix::unistd::ForkResult::Parent { child }) => {
                let mut fd_in = std::mem::MaybeUninit::<fd_set>::uninit();
                nix::libc::FD_ZERO(fd_in.as_mut_ptr());
                let mut fd_in = fd_in.assume_init();

                // Close the slave side of the PTY
                close(fds);

                loop {
                    // Wait for data from standard input and master side of PTY
                    FD_ZERO(&mut fd_in);
                    FD_SET(0, &mut fd_in);
                    FD_SET(fdm, &mut fd_in);

                    match select(fdm + 1, &mut fd_in, null_mut(), null_mut(), null_mut()) {
                        -1 => panic!("failed to select fdm + 1"),
                        _ => {
                            // If data on standard input
                            if FD_ISSET(0, &mut fd_in) {
                                match read(0, input.as_mut_ptr() as *mut c_void, input.len()) {
                                    n if n < 0 => panic!("{n}"),
                                    n => {
                                        write(fdm, input.as_ptr() as *const c_void, n as usize);
                                    }
                                }
                            }

                            // If data on master side of PTY
                            if FD_ISSET(fdm, &mut fd_in) {
                                match read(fdm, input.as_mut_ptr() as *mut c_void, input.len()) {
                                    n if n < 0 => panic!("{n}"),
                                    n => {
                                        on_output(&input);
                                        write(1, input.as_ptr() as *const c_void, n as usize);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => Err(format!("Error in fork(): {e}")),
        }
    }
}
