# Security

## Reporting a vulnerability

Please report security problems privately, through GitHub's
[Report a vulnerability](https://github.com/rikshot/rsdirstat/security/advisories/new) form, and
not in a public issue. The report is visible only to the maintainer and you, and the fix can be
prepared in private before anything is published.

## What counts

The `rsdirstat` GUI is a local web server: it listens on `127.0.0.1`, serves the file listing of
whatever it scanned, can start new scans and can open the platform file manager. It must answer only
the page it served. Anything that lets something else reach it is a vulnerability, for example:

- another website open in your browser (cross-site requests, DNS rebinding, WebSocket hijacking),
- another user on the same machine, or another machine on the network,

reading the file listing or the volume list, starting a scan, or revealing a path. So is the reveal
action opening anything other than the item that was clicked.

## Supported versions

Fixes go into the latest release. Each fix is published with a GitHub security advisory, crediting
the reporter unless they'd rather not be named.
