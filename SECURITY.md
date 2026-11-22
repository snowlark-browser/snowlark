# Security policy

Snowlark is a web browser: it runs code from any site, so security problems matter even in a preview.

## Reporting a vulnerability

Please report security problems privately by email to
[security@snowlark.org](mailto:security@snowlark.org). Do not open a public issue or pull request for them.

A useful report says:

- what the problem is and what an attacker could do with it;
- the Snowlark version (the release name or the commit) and the Windows version;
- the steps or a page that shows it, as small as you can make it;
- whether you have told anyone else, and any date you plan to publish.

We will confirm that we received your report, keep you informed while we work on it, and credit you when the fix
is published unless you would rather not be named.

## What is in scope

Anything that lets a page do what the web platform does not allow, for example:

- escaping the sandbox of a renderer process, or running code in the browser process;
- reading or changing data of another site (cookies, storage, frames, responses) across the same-origin policy
  or the site isolation between processes;
- memory corruption reachable from page content (script, HTML, CSS, images, fonts, media);
- bypassing permissions, content security policy, mixed content blocking or certificate checks;
- spoofing the address bar or other browser interface.

## Supported versions

Snowlark is in preview. Fixes go into the newest source on `main` and the next preview release; older previews
are not updated.
