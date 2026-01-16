# Contributing to FCast

First off, thank you for your interest in contributing to FCast.

## Getting Started

The FCast repository contains multiple projects, each of which has its own dependencies and setup process which are documented inside their respective directories. The main dependencies that are used across most of the projects include:
* [NPM](https://www.npmjs.com/)
* [Rust](https://rust-lang.org/)

The main projects this repository contains are:
```
├── docs          # Documentation site: https://docs.fcast.org/
├── receivers     # Receiver applications for multiple platforms
├── sdk           # Developer library packages for integrating the FCast protocol into sender apps
├── senders       # Official FCast sender applications
├── website       # Project website: https://fcast.org/
└── xtask         # Build scripts for SDK components and Rust apps
```

Its recommended to review the project-wide documentation available at https://docs.fcast.org/ depending on the component you wish to work on.

For getting started on a specific component, the readme file will provide instructions for setup and building (e.g. `receivers/electron/README.md`)

## Issues

Feel free to submit issues and enhancement requests. Existing issues such as ones labeled as `good first issue`, `help wanted`, or `bugs` are good candidates to help contribute to. If you have your own feature idea or an existing issue does not match your interest, please feel free to reach out and ask on the [FCast channel](https://chat.futo.org/#narrow/channel/67-FCast) in our FUTO Chat Zulip server before getting started.

## Making Changes

1. Fork the repository on GitHub and create a new branch in your fork: `git checkout -b my-awesome-feature`.
2. Make the changes you want to contribute.
3. Test your changes to ensure they don't break existing functionality.
4. Commit your changes. Make sure your commit messages clearly describe the changes you made.

## Submitting a Pull Request

1. Push your changes to your fork on GitHub.
2. Submit a pull request against the main FCast repository.
3. Describe your changes in the pull request. Explain what you did, how you did it, and why you did it.
4. Wait for us to review your pull request. We'll do our best to respond as promptly as we can.

## Thank You!

Again, thank you for your contribution. Your effort will make this project even better.