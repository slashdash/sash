# Security policy

## Supported versions

Sash is pre-1.0. Security fixes land on `main` and in the next test build;
older DMGs are not patched.

## Reporting a vulnerability

Do not open a public issue. Use GitHub's private vulnerability reporting:
**Security → Report a vulnerability** on
[github.com/slashdash/sash](https://github.com/slashdash/sash/security/advisories/new).

Include the build number from Settings, the macOS version, and a minimal
reproduction. You can expect an acknowledgement within a few days; this is a
volunteer project, so there is no bounty and no guaranteed fix window.

## Scope

In scope: the terminal surface (paste handling, OSC 52 and OSC 8 handling, link
opening), the pty and session code, the global hotkey and panel, and anything
that writes outside `~/.config/sash` or `~/Library/Logs/Sash`.

Out of scope: the app being ad-hoc signed rather than notarized, and
vulnerabilities in macOS, Rune or libghostty-vt that are not reachable through
Sash. Report those upstream as well.
