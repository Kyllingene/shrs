
default: debug

debug:
    cargo +nightly run shrs_example

install:
    cargo install --path shrs_example

devsetup:
    cp dev/hooks/* .git/hooks

fmt:
    cargo +nightly fmt --all

lint:
    cargo clippy -- -W clippy::unwrap_used -W clippy::cargo

docs:
    cd docs && zola serve
