//! Implementation and runtime for POSIX shell

use std::{
    collections::HashMap,
    env,
    fs::File,
    io::{stdin, stdout, BufWriter, Write},
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Output, Stdio},
    rc::Rc,
};

use anyhow::anyhow;
use crossterm::{style::Print, QueueableCommand};
use shrs_lang::{ast, Lexer, Parser, RESERVED_WORDS};
use shrs_line::{DefaultHistory, DefaultPrompt, History, Line, Prompt};

use crate::{
    alias::Alias,
    builtin::Builtins,
    env::Env,
    hooks::{AfterCommandCtx, BeforeCommandCtx, Hooks, StartupCtx},
    signal::sig_handler,
    theme::Theme,
};

/// Unified shell config struct
#[derive(Builder)]
#[builder(pattern = "owned")]
#[builder(setter(prefix = "with"))]
pub struct ShellConfig {
    #[builder(default = "Hooks::default()")]
    hooks: Hooks,

    #[builder(default = "Builtins::default()")]
    builtins: Builtins,

    #[builder(default = "Line::default()")]
    readline: Line,

    #[builder(default = "Box::new(DefaultHistory::new())")]
    #[builder(setter(custom))]
    history: Box<dyn History<HistoryItem = String>>,

    #[builder(default = "Alias::new()")]
    alias: Alias,

    /// Custom prompt
    #[builder(default = "Box::new(DefaultPrompt::new())")]
    #[builder(setter(custom))]
    prompt: Box<dyn Prompt>,

    /// Environment variables
    #[builder(default = "Env::new()")]
    env: Env,

    /// List of defined functions
    #[builder(default = "HashMap::new()")]
    functions: HashMap<String, Box<ast::Command>>,

    /// Color theme
    #[builder(default = "Theme::default()")]
    theme: Theme,
}

impl ShellConfigBuilder {
    pub fn with_prompt(mut self, prompt: impl Prompt + 'static) -> Self {
        self.prompt = Some(Box::new(prompt));
        self
    }
    pub fn with_history(mut self, history: impl History<HistoryItem = String> + 'static) -> Self {
        self.history = Some(Box::new(history));
        self
    }
}

impl ShellConfig {
    pub fn run(self) -> anyhow::Result<()> {
        // TODO some default values for Context and Runtime are duplicated by the #[builder(default = "...")]
        // calls in ShellConfigBuilder, so we are sort of defining the full default here. Maybe end
        // up implementing Default for Context and Runtime

        let mut ctx = Context {
            readline: self.readline,
            history: self.history,
            alias: self.alias,
            prompt: self.prompt,
            out: BufWriter::new(stdout()),
        };
        let mut rt = Runtime {
            env: self.env,
            working_dir: std::env::current_dir().unwrap(),
            // TODO currently hardcoded
            name: "shrs".into(),
            // TDOO currently unused (since we have not implemented functions etc)
            args: vec![],
            exit_status: 0,
            functions: self.functions,
        };
        let shell = Shell {
            builtins: self.builtins,
            hooks: self.hooks,
            theme: self.theme,
        };

        shell.run(&mut ctx, &mut rt)
    }
}

/// Constant shell data
///
/// Data here is generally not mutated at runtime.
pub struct Shell {
    pub hooks: Hooks,
    /// Builtin shell functions that have access to the shell's context
    pub builtins: Builtins,
    /// Color theme
    pub theme: Theme,
}

/// Shared global shell context
///
/// Context here is shared by each subshell
// TODO can technically unify shell and context
pub struct Context {
    pub readline: Line,
    pub history: Box<dyn History<HistoryItem = String>>,
    // TODO alias is currently unused
    pub alias: Alias,
    /// Custom prompt
    pub prompt: Box<dyn Prompt>,
    /// Output stream
    pub out: BufWriter<std::io::Stdout>,
}

/// Runtime context for the shell
///
/// Contains data that can should be local to each subshell. Data here should also be able to be
/// cloned.
#[derive(Clone)]
pub struct Runtime {
    /// Current working directory
    pub working_dir: PathBuf,
    /// Environment variables
    pub env: Env,
    /// Name of the shell or shell script
    pub name: String,
    /// Arguments this shell was called with
    pub args: Vec<String>,
    /// Exit status of most recent pipeline
    pub exit_status: i32,
    /// List of defined functions
    pub functions: HashMap<String, Box<ast::Command>>,
}

impl Shell {
    pub fn run(&self, ctx: &mut Context, rt: &mut Runtime) -> anyhow::Result<()> {
        // init stuff
        sig_handler()?;
        rt.env.load();

        self.hooks
            .startup
            .run(&mut ctx.out, &StartupCtx { startup_time: 0 });

        loop {
            let line = ctx.readline.read_line(&ctx.prompt);

            // attempt to expand alias
            let expanded = ctx.alias.get(&line).unwrap_or(&line.to_string()).clone();

            // TODO not sure if hook should run here (since not all vars are expanded yet)
            let hook_ctx = BeforeCommandCtx {
                raw_command: line.clone(),
                command: expanded.clone(),
            };
            self.hooks.before_command.run(&mut ctx.out, &hook_ctx)?;

            // TODO rewrite the error handling here better
            let lexer = Lexer::new(&expanded);
            let mut parser = Parser::new();
            let cmd = match parser.parse(lexer) {
                Ok(cmd) => cmd,
                Err(e) => {
                    eprintln!("{}", e);
                    continue;
                },
            };
            let cmd_handle =
                match self.eval_command(ctx, rt, &cmd, Stdio::inherit(), Stdio::piped(), None) {
                    Ok(cmd_handle) => cmd_handle,
                    Err(e) => {
                        eprintln!("{}", e);
                        continue;
                    },
                };
            self.command_output(ctx, rt, cmd_handle)?;
        }
    }

    // TODO function signature is very ugly
    // TODO maybe make this a method of Command
    pub fn eval_command(
        &self,
        ctx: &mut Context,
        rt: &mut Runtime,
        cmd: &ast::Command,
        stdin: Stdio,
        stdout: Stdio,
        pgid: Option<i32>,
    ) -> anyhow::Result<Child> {
        match cmd {
            ast::Command::Simple {
                assigns,
                args,
                redirects,
            } => {
                if args.len() == 0 {
                    return Err(anyhow!("command is empty"));
                }
                // println!("redirects {:?}", redirects);
                // println!("assigns {:?}", assigns);

                // file redirections
                // TODO: current behavior, only one read and write operation is allowed, the latter ones will override the behavior of eariler ones
                let mut cur_stdin = stdin;
                let mut cur_stdout = stdout;
                for redirect in redirects {
                    let filename = Path::new(&*redirect.file);
                    // TODO not making use of file descriptor at all right now
                    let n = match &redirect.n {
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
                let cmd_name = &it.next().unwrap();
                let args = it.map(|a| (*a).clone()).collect::<Vec<_>>();

                // TODO which stdin var to use?, previous command or from file redirection?

                // TODO doing args subst here is a waste if we evaluating function body
                let subst_args = args.iter().map(|x| envsubst(rt, x)).collect::<Vec<_>>();

                for (builtin_name, builtin_cmd) in self.builtins.builtins.iter() {
                    if builtin_name == &cmd_name.as_str() {
                        return builtin_cmd.run(ctx, rt, &subst_args);
                    }
                }

                // otherwise look for defined functions
                let cmd_body = rt.functions.get(cmd_name.as_str()).cloned();
                match cmd_body {
                    Some(ref cmd_body) => {
                        self.eval_command(ctx, rt, cmd_body, Stdio::inherit(), Stdio::piped(), None)
                    },
                    None => self.run_external_command(
                        ctx,
                        rt,
                        &cmd_name,
                        &subst_args,
                        cur_stdin,
                        cur_stdout,
                        None,
                        assigns,
                    ),
                }
            },
            ast::Command::Pipeline(a_cmd, b_cmd) => {
                // TODO double check that pgid works properly for pipelines that are longer than one pipe (left recursiveness of parser might mess this up)
                let mut a_cmd_handle =
                    self.eval_command(ctx, rt, a_cmd, stdin, Stdio::piped(), None)?;
                let piped_stdin = Stdio::from(a_cmd_handle.stdout.take().unwrap());
                let pgid = a_cmd_handle.id();
                let b_cmd_handle =
                    self.eval_command(ctx, rt, b_cmd, piped_stdin, stdout, Some(pgid as i32))?;
                Ok(b_cmd_handle)
            },
            ast::Command::Or(a_cmd, b_cmd) | ast::Command::And(a_cmd, b_cmd) => {
                let negate = match cmd {
                    ast::Command::Or { .. } => false,
                    ast::Command::And { .. } => true,
                    _ => unreachable!(),
                };
                // TODO double check if these stdin and stdou params are correct
                let a_cmd_handle =
                    self.eval_command(ctx, rt, a_cmd, Stdio::inherit(), Stdio::piped(), None)?;
                if let Some(output) = self.command_output(ctx, rt, a_cmd_handle)? {
                    if output.status.success() ^ negate {
                        // TODO return something better (indicate that command failed with exit code)
                        return dummy_child();
                    }
                }
                let b_cmd_handle =
                    self.eval_command(ctx, rt, b_cmd, Stdio::inherit(), Stdio::piped(), None)?;
                Ok(b_cmd_handle)
            },
            ast::Command::Not(cmd) => {
                // TODO exit status negate
                let cmd_handle = self.eval_command(ctx, rt, cmd, stdin, stdout, None)?;
                Ok(cmd_handle)
            },
            ast::Command::AsyncList(a_cmd, b_cmd) => {
                let a_cmd_handle =
                    self.eval_command(ctx, rt, a_cmd, Stdio::inherit(), Stdio::piped(), None)?;

                match b_cmd {
                    None => Ok(a_cmd_handle),
                    Some(b_cmd) => {
                        let b_cmd_handle = self.eval_command(
                            ctx,
                            rt,
                            b_cmd,
                            Stdio::inherit(),
                            Stdio::piped(),
                            None,
                        )?;
                        Ok(b_cmd_handle)
                    },
                }
            },
            ast::Command::SeqList(a_cmd, b_cmd) => {
                // TODO very similar to AsyncList
                let a_cmd_handle =
                    self.eval_command(ctx, rt, a_cmd, Stdio::inherit(), Stdio::piped(), None)?;

                match b_cmd {
                    None => Ok(a_cmd_handle),
                    Some(b_cmd) => {
                        self.command_output(ctx, rt, a_cmd_handle)?;
                        let b_cmd_handle = self.eval_command(
                            ctx,
                            rt,
                            b_cmd,
                            Stdio::inherit(),
                            Stdio::piped(),
                            None,
                        )?;
                        Ok(b_cmd_handle)
                    },
                }
            },
            ast::Command::Subshell(cmd) => {
                // TODO rn history is being copied too, history (and also alias?) really should be global
                // maybe seperate out global context and runtime context into two structs?
                let mut new_rt = rt.clone();
                let cmd_handle = self.eval_command(
                    ctx,
                    &mut new_rt,
                    cmd,
                    Stdio::inherit(),
                    Stdio::piped(),
                    None,
                )?;
                Ok(cmd_handle)
            },
            ast::Command::If { conds, else_part } => {
                // TODO throw proper error here
                assert!(conds.len() >= 1);

                for ast::Condition { cond, body } in conds {
                    let cond_handle =
                        self.eval_command(ctx, rt, cond, Stdio::inherit(), Stdio::piped(), None)?;
                    // TODO sorta similar to and statements
                    if let Some(output) = self.command_output(ctx, rt, cond_handle)? {
                        if output.status.success() {
                            let body_handle = self.eval_command(
                                ctx,
                                rt,
                                body,
                                Stdio::inherit(),
                                Stdio::piped(),
                                None,
                            )?;
                            return Ok(body_handle);
                        }
                    }
                }

                if let Some(else_part) = else_part {
                    let else_handle = self.eval_command(
                        ctx,
                        rt,
                        else_part,
                        Stdio::inherit(),
                        Stdio::piped(),
                        None,
                    )?;
                    return Ok(else_handle);
                }

                dummy_child()
            },
            ast::Command::While { cond, body } | ast::Command::Until { cond, body } => {
                let negate = match cmd {
                    ast::Command::While { .. } => false,
                    ast::Command::Until { .. } => true,
                    _ => unreachable!(),
                };

                loop {
                    let cond_handle =
                        self.eval_command(ctx, rt, cond, Stdio::inherit(), Stdio::piped(), None)?;
                    // TODO sorta similar to if statements
                    if let Some(output) = self.command_output(ctx, rt, cond_handle)? {
                        if output.status.success() ^ negate {
                            let body_handle = self.eval_command(
                                ctx,
                                rt,
                                body,
                                Stdio::inherit(),
                                Stdio::piped(),
                                None,
                            )?;
                            self.command_output(ctx, rt, body_handle)?;
                        } else {
                            break;
                        }
                    } else {
                        break; // TODO not sure if there should be break here
                    }
                }
                dummy_child()
            },
            ast::Command::For {
                name,
                wordlist,
                body,
            } => {
                // expand wordlist
                let mut expanded = vec![];
                for word in wordlist {
                    // TODO use IFS variable for this
                    for subword in word.split(" ") {
                        expanded.push(subword);
                    }
                }

                // execute body
                for word in expanded {
                    // TODO should have seperate variable struct instead of env
                    rt.env.set(name, word); // TODO unset the var after the loop?
                    let body_handle =
                        self.eval_command(ctx, rt, body, Stdio::inherit(), Stdio::piped(), None)?;
                    self.command_output(ctx, rt, body_handle)?;
                }

                dummy_child()
            },
            ast::Command::Case { word, arms } => {
                // println!("word {:?}, arms {:?}", word, arms);

                let subst_word = envsubst(rt, word);

                for ast::CaseArm { pattern, body } in arms {
                    if pattern.iter().any(|x| x == &subst_word) {
                        let body_handle = self.eval_command(
                            ctx,
                            rt,
                            body,
                            Stdio::inherit(),
                            Stdio::piped(),
                            None,
                        )?;
                        self.command_output(ctx, rt, body_handle)?;
                        // TODO should we break? (should multiple match arms be matched?)
                    }
                }

                dummy_child()
            },
            ast::Command::Fn { fname, body } => {
                if RESERVED_WORDS.contains(&fname.as_str()) {
                    eprintln!("function nane cannot be a reserved keyword");
                    return dummy_child(); // TODO come up with better return value
                }

                // TODO hook for redefining function?
                rt.functions.insert(fname.to_string(), body.to_owned());

                dummy_child()
            },
            ast::Command::None => dummy_child(),
        }
    }

    fn run_external_command(
        &self,
        ctx: &mut Context,
        rt: &mut Runtime,
        cmd: &str,
        args: &Vec<String>,
        stdin: Stdio,
        stdout: Stdio,
        pgid: Option<i32>,
        assigns: &Vec<ast::Assign>,
    ) -> anyhow::Result<Child> {
        use std::process::Command;

        let envs = assigns.iter().map(|word| (&word.var, &word.val));

        // TODO might need to do subst on cmd too
        let child = Command::new(cmd)
            .args(args)
            .stdin(stdin)
            .stdout(stdout)
            .process_group(pgid.unwrap_or(0)) // pgid of 0 means use own pid as pgid
            .current_dir(rt.working_dir.to_str().unwrap())
            .envs(envs)
            .spawn()?;

        Ok(child)
    }

    /// Small wrapper that outputs command output if exists
    fn command_output(
        &self,
        ctx: &mut Context,
        rt: &mut Runtime,
        cmd_handle: Child,
    ) -> anyhow::Result<Option<Output>> {
        let cmd_output = cmd_handle.wait_with_output()?;
        let utf8_output = std::str::from_utf8(&cmd_output.stdout)?;

        ctx.out.queue(Print(utf8_output))?;

        let exit_code = cmd_output.status.code().unwrap();
        rt.exit_status = exit_code;

        let hook_ctx = AfterCommandCtx {
            exit_code,
            cmd_time: 0.0,
        };
        self.hooks.after_command.run(&mut ctx.out, &hook_ctx)?;

        ctx.out.flush()?;
        Ok(Some(cmd_output))
    }
}

pub fn dummy_child() -> anyhow::Result<Child> {
    use std::process::Command;
    let cmd = Command::new("true").spawn()?;
    Ok(cmd)
}

/// Performs environment substition on a string
// TODO regex replace might not be the best way. could also recognize the env var during parsing
// TODO handle escaped characters
fn envsubst(rt: &mut Runtime, arg: &str) -> String {
    use regex::Regex;

    let mut subst = arg.to_string();

    // substitute special parameters first
    subst = subst.as_str().replace("$?", &rt.exit_status.to_string());
    subst = subst.as_str().replace("$#", &rt.args.len().to_string());
    subst = subst.as_str().replace("$0", &rt.name);

    // TODO precompile regex in lazy_static
    let r_0 = Regex::new(r"\$(?P<env>[a-zA-Z_]+)").unwrap(); // no braces
    let r_1 = Regex::new(r"\$\{(?P<env>[a-zA-Z_]+)\}").unwrap(); // with braces
    let r_2 = Regex::new(r"~").unwrap(); // tilde

    for cap in r_0.captures_iter(arg) {
        // look up env var
        let var = &cap["env"];
        // TODO stupid code
        let val = match rt.env.get(var) {
            Some(val) => val.clone(),
            None => String::new(),
        };
        let fmt_env = format!("${}", var); // format $VAR
        subst = subst.as_str().replace(&fmt_env, &val);
    }

    // TODO this is dumb stupid and bad repeated code
    for cap in r_1.captures_iter(arg) {
        let var = &cap["env"];
        let val = match rt.env.get(var) {
            Some(val) => val.clone(),
            None => String::new(),
        };
        let fmt_env = format!("${{{}}}", var); // format ${VAR}
        subst = subst.as_str().replace(&fmt_env, &val);
    }

    // tilde substitution
    let home = match rt.env.get("HOME") {
        Some(home) => home.as_str(),
        None => "",
    };
    let subst = r_2.replace_all(&subst, home).to_string();

    subst
}

/// Looks through each directory in path and finds executables
pub fn find_executables_in_path(path_str: &str) -> Vec<String> {
    use std::{fs, os::unix::fs::PermissionsExt};

    let mut execs = vec![];
    for path in path_str.split(":") {
        let dir = match fs::read_dir(path) {
            Ok(dir) => dir,
            Err(_) => continue,
        };
        for file in dir {
            if let Ok(dir_entry) = file {
                // check if file is executable
                if dir_entry.metadata().unwrap().permissions().mode() & 0o111 != 0 {
                    execs.push(dir_entry.file_name().to_str().unwrap().into());
                }
            }
        }
    }
    execs
}

#[cfg(test)]
mod tests {
    use super::{envsubst, Runtime};
    use crate::shell::find_executables_in_path;

    // #[test]
    // fn envsubst_test() {
    //     let mut rt = Runtime::default();
    //     rt.env.set("EDITOR", "vim");
    //     rt.env.set("SHELL", "/bin/shrs");
    //     let text = "$SHELL ${EDITOR}";
    //     let subst = envsubst(&mut rt, text);
    //     assert_eq!(subst, String::from("/bin/shrs vim"));
    // }

    #[test]
    fn path_execs_test() {
        println!("{:?}", find_executables_in_path("/usr/bin:/usr/local/bin"));
    }
}
