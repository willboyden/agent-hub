# Security policy

## Reporting a vulnerability

Please **do not open a public issue** for a security problem. Use GitHub's private vulnerability reporting: on the
repository page choose Security, then "Report a vulnerability" (a private security advisory). Include the version or commit,
what you did, what you expected, and what happened. Do not include real credentials.

You can expect an acknowledgement, then a fix or a written assessment. This is a small, volunteer-run project: there is no
guaranteed response time.

## Scope

In scope: the hub backend and `hubctl`, the web UI, the policy floor logic, path and delivery safety, the knowledge service
and its broker addon, and the deploy templates.

Out of scope, by design: a process running as the **same OS user** as the hub (it can read whatever that user can), an
unsandboxed agent client running as that user, and host-level hardening choices. These are described honestly in
`docs/SECURITY.md` (residual risks). Reports that only restate them are welcome as documentation fixes, not as
vulnerabilities.

## Supported versions

Only the latest release (currently 0.1.x) receives fixes. The project is pre-1.0; interfaces and the content schema may change.

## Threat model

`docs/SECURITY.md` has the STRIDE table, trust boundaries and residual risks. Nothing in it should be read as a guarantee.
