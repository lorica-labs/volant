# Security policy

## Supported versions

Volant is in pre-release. Security fixes go into the next release, and only the latest release is supported.

## Reporting a vulnerability

Use GitHub's private vulnerability reporting: open the **Security** tab of this repository and choose **Report a vulnerability**. If that is not possible, write to <hugo.planque02@gmail.com>.

You will get an acknowledgment within 7 days. Once a fix is ready, we publish a security advisory that credits the reporter, unless you prefer to stay anonymous.

Please do not open public issues for security problems.

## What counts as a vulnerability here

Volant runs playbooks the way ansible-core does, so some of its bugs are differences in behavior. A difference that lets data from a managed host act on the controller is not a compatibility bug. Report it here rather than in the issue tracker.

Templating is the clearest case. Text that comes from a host is data: a command's output, a registered value, a gathered fact, a file read during the run. If Volant evaluates that text as a Jinja expression where ansible-core would print it, a host can run code on the machine driving the run.

The same reasoning covers:

- a secret that reaches a log or a terminal where `no_log` hides it in ansible-core;
- an argument a host controls reaching a shell;
- a host changing its own connection settings, such as its address, user or SSH arguments, through the facts a module returns.
