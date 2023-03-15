use std::default;

use shrs::{
    alias::Alias,
    builtin::Builtins,
    env::Env,
    prompt::{hostname, top_pwd, username},
    shell::{self, find_executables_in_path, Context, Runtime, ShellConfig},
};
use shrs_line::{line::Line, prompt::Prompt};

struct MyPrompt;

impl Prompt for MyPrompt {
    fn prompt_left(&self) -> String {
        format!(" {} > ", top_pwd())
    }
}

fn main() {
    use shell::ShellConfigBuilder;

    let mut env = Env::new();
    env.load();

    let completions: Vec<String> = find_executables_in_path(env.get("PATH").unwrap());
    let completer = shrs_line::completion::DefaultCompleter::new(completions);
    let menu = shrs_line::menu::DefaultMenu::new();
    let history = shrs_line::history::DefaultHistory::new();
    let cursor = shrs_line::cursor::DefaultCursor::default();

    let prompt = MyPrompt;
    let readline = Line::new(menu, completer, history, cursor);

    let alias = Alias::from_iter([
        ("l".into(), "ls".into()),
        ("c".into(), "cd".into()),
        ("g".into(), "git".into()),
        ("v".into(), "vim".into()),
        ("la".into(), "ls -a".into()),
    ]);

    let myshell = ShellConfigBuilder::default()
        .with_env(env)
        .with_alias(alias)
        .with_readline(readline)
        .with_prompt(prompt)
        .build()
        .unwrap();

    myshell.run();
}
