# Contributing to Splitstream

Thank you for your interest in contributing to Splitstream! This document provides guidelines for contributing to the project.

## Welcome

We welcome contributions of all kinds:
- Bug reports and feature requests
- Documentation improvements
- Code contributions (bug fixes, new features, optimizations)
- Testing and feedback

Testing feedback is worth calling out first. Splitstream is a realtime audio
router, and its hardest defects only appear against real hardware over long
sessions — different DAC sample rates, device add/remove, several apps routed at
once. A careful bug report with an audit trace (below) is often more valuable
than a patch.

## Ground Rules & Expectations

### Maintainer Authority

All contributions are subject to final approval by the project maintainer. The maintainer:
- Has final say on accepting or rejecting any contribution
- May request changes, clarifications, or alternative approaches
- Makes decisions based on project vision, code quality, and long-term maintainability

### Review Process

- Pull request reviews happen when the maintainer is available
- There are no guaranteed response timeframes
- Please be patient and respectful while waiting for review
- The maintainer may request changes or provide feedback

### AI-Assisted Development

AI tools (ChatGPT, Claude, GitHub Copilot, etc.) are **encouraged and welcomed** for contributions.

**Requirements:**
- You must be able to explain:
  - What your changes do
  - Why you made them
  - How the code works
- AI-generated code must meet all quality standards (tests, clippy)

**Optional but helpful:**
- Mention which AI tools you used in your PR description
- This helps provide context during code review

## Development Setup

### Prerequisites

- Windows — Splitstream is Windows-only by design (WASAPI, per-process loopback
  capture, tray/shell integration). It does not build or run on Linux or macOS.
- A recent stable Rust toolchain (install from [rustup.rs](https://rustup.rs)).
  The project is developed against Rust 1.93 with the MSVC toolchain; there is
  no formally supported MSRV yet.
- Cargo (comes with Rust)
- A virtual audio device to act as the silent sink — free
  [VB-CABLE](https://vb-audio.com/Cable/) is the default recommendation. Anything
  already installed (SteelSeries Sonar, VoiceMeeter, or a physical output you
  never listen to) works identically, since the sink is chosen by device name.
  See the README for why this is needed.
- [Inno Setup](https://jrsoftware.org/isinfo.php) only if you want to build the
  installer (`installer/splitstream.iss`).

### Getting Started

1. Clone the repository:
   ```bash
   git clone https://github.com/nschroeder8472/Splitstream.git
   cd Splitstream
   ```
2. Build the project:
   ```bash
   cargo build
   ```
3. Run tests:
   ```bash
   cargo test --workspace
   ```
4. Run it:
   ```bash
   cargo run --release
   ```
   Release matters for anything you intend to listen to — a debug build carries
   enough overhead in the mixer to change what you hear.

### The audit trace

Setting `SPLITSTREAM_AUDIT=1` makes the app log one `audit` line per second with
the flow-control state: ring fills, applied resample ratio, per-group and
per-output peaks, drop and xrun counters, and why an output ring last rejected a
push. It is off by default and is the single most useful artifact when
diagnosing anything audible.

```powershell
$env:SPLITSTREAM_AUDIT = "1"
target\release\splitstream.exe
```

Logs are written to `%APPDATA%\Splitstream\logs\`.

## How to Contribute

### Reporting Bugs

Before submitting a bug report:
1. Check existing issues to avoid duplicates
2. Gather relevant information (Rust version, Windows version, configuration)

When submitting:
- Use a clear, descriptive title
- Provide detailed steps to reproduce the issue
- Include error messages, logs, or screenshots if applicable
- Specify your environment:
  - Rust version (`rustc --version`)
  - Windows version and build (`winver`)
  - Your output device and its sample rate (Sound → device → Advanced)
  - Which virtual audio device you use as the sink

For anything audible — dropouts, static, popping, silence — please also attach
15–20 consecutive `audit` lines captured while the symptom is happening. Those
counters distinguish starvation from truncation from a routing miss, which
descriptions of the sound alone cannot.

### Suggesting Features

Before suggesting a feature:
1. Check existing issues and discussions
2. Consider if it fits the project's scope

When suggesting:
- Use a clear, descriptive title
- Explain the use case and benefits
- Describe the desired behavior
- Be open to feedback and alternative approaches

Note that some things are deliberate non-goals: pro-audio sub-5 ms latency,
cross-platform support, and shipping a kernel-mode audio driver. See §2.2 of the
engineering spec before proposing those.

### Code Contributions

#### Before You Start

1. **Check existing issues** - Someone may already be working on it
2. **Discuss major changes** - Open an issue first for significant features or refactors
3. **Create a feature branch** - Branch from `main` with a descriptive name:
   ```bash
   git checkout -b feature/your-feature-name
   # or
   git checkout -b fix/bug-description
   ```

#### Coding Standards

All code contributions must meet these requirements:

**Quality Checks:**
- Tests must pass: `cargo test --workspace`
- Linting must pass: `cargo clippy --workspace --all-targets` (zero warnings)

**On formatting:** the tree is deliberately not `rustfmt`-formatted, so please
do **not** run `cargo fmt` across it — it would produce a large diff unrelated to
your change and make review impossible. Match the style of the code around you.

**Code Guidelines:**
- Follow existing code patterns and style
- Add unit tests for new functionality
- Update documentation (README, the engineering spec, `.lattice/context/`) as needed
- Keep changes focused and avoid unrelated modifications
- Use meaningful variable and function names
- Comment the *why*, not the *what* — this codebase's comments carry the
  measurements and dead ends behind a decision, so the next person does not
  re-derive them. Match that.

**Architecture:**
- Reference `Splitstream-Engineering-Spec.md` for the architecture overview, and
  `.lattice/context/` for per-feature design records
- Respect the crate boundaries: `audio-core` (pure DSP/mixing, no Windows APIs),
  `engine` (graph, threads, flow control), `win-audio` (WASAPI), `win-shell`
  (tray/shell), `control` (routing/config), `app` (UI and wiring)
- Keep Windows-specific code out of `audio-core` — its portability is what makes
  the DSP and mixer unit-testable
- Nothing on the realtime path may allocate, lock, or log. Buffers are
  preallocated at graph build time; failures are counted, never printed

**Realtime audio changes** (mixer, resampler, flow control, capture/render
loops) carry a higher bar, because a plausible-sounding change that compiles can
still be silently wrong for hours:
- Pin the behaviour with a test that fails when the one line under suspicion is
  reverted — an offline oracle beats an argument
- State what you measured, not what you expect. Counters and A/B numbers, not
  "should be fine"
- Say plainly whether you verified it on real hardware, and with what device
  rates. Both answers are acceptable; silence is not

#### Commit Messages

- Use clear, descriptive commit messages
- Format: Start with a verb (Add, Fix, Update, Refactor, etc.)
- Reference issues when applicable: "Fix capture ring saturation (#123)"
- Examples:
  - "Add per-group spatial audio toggle"
  - "Fix output ring truncation at mismatched device rates"
  - "Update level meter ballistics"

#### Pull Request Process

1. **Ensure quality checks pass locally:**
   ```bash
   cargo test --workspace
   cargo clippy --workspace --all-targets
   ```

2. **Update documentation** if needed:
   - README.md for user-facing changes
   - `Splitstream-Engineering-Spec.md` for architecture changes
   - `.lattice/context/` for design decisions and their reasoning
   - Code comments for complex logic

3. **Fill out PR description** with:
   - **What changed:** Brief summary of modifications
   - **Why it changed:** Problem being solved or feature being added
   - **How to test it:** Steps to verify the changes work
   - **Hardware verification:** For audio-path changes, what you heard and what
     the counters said
   - **(Optional) AI tools used:** Mention if you used AI assistance

4. **Be responsive to feedback:**
   - Address review comments promptly
   - Ask questions if feedback is unclear
   - Be open to requested changes

5. **Maintainer review:**
   - The maintainer will review when available
   - May approve, request changes, or close the PR
   - Final decision rests with the maintainer

## Code of Conduct

This project adheres to a Code of Conduct (see [CODE_OF_CONDUCT](CODE_OF_CONDUCT.md)). By participating, you are expected to uphold this code. Please report unacceptable behavior by opening an issue or contacting the maintainer.

## Getting Help

- **Questions?** Open a GitHub Discussion or Issue
- **Documentation:** Check README.md and `Splitstream-Engineering-Spec.md`

## Recognition

Contributors will be acknowledged in release notes and project documentation. Thank you for helping make Splitstream better!
