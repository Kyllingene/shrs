use std::{
    env,
    fs::File,
    io::{stdin, stdout, Write},
    path::Path,
    process::{Child, Output, Stdio},
};

use anyhow::anyhow;

use crate::{ast, parser};

pub fn simple_prompt() {
    print!("> ");
    stdout().flush();
}
pub fn simple_error() {}

pub type PromptCommand = fn();
pub type ErrorCommand = fn();
pub struct Shell {
    /// User defined command that gets ran when we wish to print the prompt
    pub prompt_command: PromptCommand,
    /// User defined command for formatting shell error messages
    pub error_command: ErrorCommand,
}

impl Shell {
    // pub fn new() -> Self {
    //     Shell {}
    // }

    pub fn run(&mut self) -> anyhow::Result<()> {
        loop {
            (self.prompt_command)();

            let mut line = String::new();
            if let Err(e) = stdin().read_line(&mut line) {
                continue;
            }

            let mut parser = parser::ParserContext::new();
            match parser.parse(&line) {
                Ok(cmd) => {
                    let cmd_handle = self.eval_command(cmd, Stdio::inherit(), Stdio::piped())?;
                    command_output(cmd_handle)?;
                },
                Err(e) => {
                    eprintln!("{}", e);
                },
            }
        }
    }

    // TODO function signature is very ugly
    // TODO maybe make this a method of Command
    pub fn eval_command(
        &mut self,
        cmd: ast::Command,
        stdin: Stdio,
        stdout: Stdio,
    ) -> anyhow::Result<Child> {
        match cmd {
            ast::Command::Simple { args, redirects } => {
                if args.len() == 0 {
                    return Err(anyhow!("command is empty"));
                }
                // println!("redirects {:?}", redirects);

                // file redirections
                // TODO: current behavior, only one read and write operation is allowed, the latter ones will override the behavior of eariler ones
                let mut cur_stdin = stdin;
                let mut cur_stdout = stdout;
                for redirect in redirects {
                    let filename = Path::new(&*redirect.file);
                    // TODO not making use of file descriptor at all right now
                    let n = match redirect.n {
                        Some(n) => *n,
                        None => 1,
                    };
                    match redirect.mode {
                        ast::RedirectMode::Read => {
                            let file_handle = File::options().read(true).open(filename).unwrap();
                            cur_stdin = Stdio::from(file_handle);
                        },
                        ast::RedirectMode::Write => {
                            let file_handle = File::options()
                                .write(true)
                                .create_new(true)
                                .open(filename)
                                .unwrap();
                            cur_stdout = Stdio::from(file_handle);
                        },
                        ast::RedirectMode::ReadAppend => {
                            let file_handle = File::options()
                                .read(true)
                                .append(true)
                                .open(filename)
                                .unwrap();
                            cur_stdin = Stdio::from(file_handle);
                        },
                        ast::RedirectMode::WriteAppend => {
                            let file_handle = File::options()
                                .write(true)
                                .append(true)
                                .create_new(true)
                                .open(filename)
                                .unwrap();
                            cur_stdout = Stdio::from(file_handle);
                        },
                        ast::RedirectMode::ReadDup => {
                            unimplemented!()
                        },
                        ast::RedirectMode::WriteDup => {
                            unimplemented!()
                        },
                        ast::RedirectMode::ReadWrite => {
                            let file_handle = File::options()
                                .read(true)
                                .write(true)
                                .create_new(true)
                                .open(filename)
                                .unwrap();
                            cur_stdin = Stdio::from(file_handle.try_clone().unwrap());
                            cur_stdout = Stdio::from(file_handle);
                        },
                    };
                }

                let mut it = args.into_iter();
                let cmd_name = it.next().unwrap().0;
                let args = it
                    .collect::<Vec<_>>()
                    .into_iter()
                    .map(|a| a.clone())
                    .collect();

                // TODO which stdin var to use?, previous command or from file redirection?

                match cmd_name.as_str() {
                    "cd" => self.run_cd_command(&args),
                    "exit" => self.run_exit_command(&args),
                    _ => self.run_external_command(&cmd_name, &args, cur_stdin, cur_stdout),
                }
            },
            ast::Command::Pipeline(a_cmd, b_cmd) => {
                let mut a_cmd_handle = self.eval_command(*a_cmd, stdin, Stdio::piped())?;
                let piped_stdin = Stdio::from(a_cmd_handle.stdout.take().unwrap());
                let b_cmd_handle = self.eval_command(*b_cmd, piped_stdin, stdout)?;
                Ok(b_cmd_handle)
            },
            ast::Command::And(a_cmd, b_cmd) => {
                // TODO double check if these stdin and stdou params are correct
                let a_cmd_handle = self.eval_command(*a_cmd, Stdio::inherit(), Stdio::piped())?;
                if let Some(output) = command_output(a_cmd_handle)? {
                    if !output.status.success() {
                        // TODO return something better (indicate that command failed with exit code)
                        return dummy_output();
                    }
                }
                let b_cmd_handle = self.eval_command(*b_cmd, Stdio::inherit(), Stdio::piped())?;
                Ok(b_cmd_handle)
            },
            // duplicate of And (could abstract a bit)
            ast::Command::Or(a_cmd, b_cmd) => {
                let a_cmd_handle = self.eval_command(*a_cmd, Stdio::inherit(), Stdio::piped())?;
                if let Some(output) = command_output(a_cmd_handle)? {
                    if output.status.success() {
                        return dummy_output();
                    }
                }
                let b_cmd_handle = self.eval_command(*b_cmd, Stdio::inherit(), Stdio::piped())?;
                Ok(b_cmd_handle)
            },
            ast::Command::Not(cmd) => {
                // TODO exit status negate
                let cmd_handle = self.eval_command(*cmd, stdin, stdout)?;
                Ok(cmd_handle)
            },
        }
    }

    fn run_cd_command(&self, args: &Vec<String>) -> anyhow::Result<Child> {
        // if empty default to root (for now)
        let raw_path = if let Some(path) = args.get(0) {
            path
        } else {
            "/"
        };
        let path = Path::new(raw_path);
        env::set_current_dir(path)?;

        // return a dummy command
        dummy_output()
    }

    fn run_exit_command(&self, args: &Vec<String>) -> ! {
        std::process::exit(0)
    }

    fn run_external_command(
        &self,
        cmd: &str,
        args: &Vec<String>,
        stdin: Stdio,
        stdout: Stdio,
    ) -> anyhow::Result<Child> {
        use std::process::Command;

        let child = Command::new(cmd)
            .args(args)
            .stdin(stdin)
            .stdout(stdout)
            .spawn()?;
        Ok(child)
    }
}

/// Small wrapper that outputs command output if exists
fn command_output(cmd_handle: Child) -> anyhow::Result<Option<Output>> {
    let cmd_output = cmd_handle.wait_with_output()?;
    // println!("[exit +{}]", cmd_output.status);
    print!("{}", std::str::from_utf8(&cmd_output.stdout)?);
    stdout().flush()?;
    Ok(Some(cmd_output))
}

fn dummy_output() -> anyhow::Result<Child> {
    use std::process::Command;
    let cmd = Command::new("true").spawn()?;
    Ok(cmd)
}
