# Git Bamboo

A fast, native Git integration for Visual Studio Code, powered by a Rust engine.

## Features

- **Commit graph** — interactive visualization of your repository's branch and commit history
- **Commit details** — inspect diffs and file changes for any commit
- **Working tree view** — review, stage, and commit changes
- **Native performance** — all Git operations run in a bundled Rust engine (`bamboo-engine`), no external Git process required

## Requirements

- Visual Studio Code 1.90.0 or newer
- Platform-specific builds are published for Windows (x64), macOS (Apple Silicon), and Linux (x64)

## Install

Each GitHub Release attaches one VSIX per platform (`TargetPlatform` is inside the package). Install with **Extensions: Install from VSIX...** and pick the file for your OS:

- Windows x64: `git-bamboo-win32-x64-<version>.vsix`
- macOS Apple Silicon: `git-bamboo-darwin-arm64-<version>.vsix`
- Linux x64: `git-bamboo-linux-x64-<version>.vsix`

## Usage

After installation, open a folder containing a Git repository. Git Bamboo contributes:

- A **Git Bamboo** view in the Source Control activity bar
- Commands via the Command Palette (`Ctrl+Shift+P` / `Cmd+Shift+P`): commit graph, commit, and more

## Repository

Source code: [https://github.com/pandas-coding/git-bamboo](https://github.com/pandas-coding/git-bamboo)

Please report issues on the GitHub issue tracker.

## License

Dual-licensed under MIT OR Apache-2.0.
