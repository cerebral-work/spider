# crawllab bench

Fork-only. Runs spider the way the crawlers use it, driving a remote obscura
browser over CDP, against local fixtures, with every service in a container on
one private network. It is the regression check for the remote-browser fixes;
run it before shipping a spider or obscura change.

```bash
OBSCURA_BIN_DIR=/dir/with/linux/obscura ./run.sh
```

`OBSCURA_BIN_DIR` holds a Linux `obscura` binary built with `--features stealth`.
Set `OBSCURA_PROFILE=/path/device.json` to run it with a device profile.

## Checks

| Check | What passes |
|---|---|
| `anubis` | Every Anubis challenge method (metarefresh, preact, fast, slow, sha256, argon2id, hashx) at base and hard settings ends on the backend page, classified `Content` |
| `refresh` | A page that refreshes to another page or to a 404 reports that document's URL and status |
| `dns` | A hostname only the browser can resolve still loads (no local-DNS veto) |
| `session` | Two crawls sharing a cookie jar: Anubis challenges only the first |
| `challenge-page` | A captured Anubis challenge page classifies as `Challenge`, and its honeypot link is not extracted |
| pointer | No `Input.dispatchMouseEvent` reaches the browser with `PointerAssist` off |
| identity | Every request at the backend carries one user agent and platform |

The runner's crawler configuration is the reference for remote-browser use: the
browser owns the identity (`with_user_agent(None)`, stealth and fingerprint
off), `PointerAssist` is off, and a page wait lets self-navigating pages settle.
Browser pages and the HTTP client share `Website::cookie_jar`, so crawls given
one jar, or one `Website` crawled again, continue the session on each host they
crawl. Cookies going from the jar into the browser carry name and value only,
and the jar lives in memory.

## Fixtures

- `fixtures/echo.py`: the backend. It logs each request's identity headers and
  serves refresh, 404 and content routes.
- `fixtures/anubis-policy.yaml`: one challenge method per path. The JavaScript
  `fast` method stops at difficulty 5. Its solve time is random and rises with
  host load: at 5 it took 3 to 33 s on a loaded Mac (from Anubis's logs), so
  that check waits 90 s. At 6 it is too slow to bench, and `hard/sha256-6`
  covers difficulty 6.

Images: `ghcr.io/techarohq/anubis:v1.28.2`, `python:3.12-slim`,
`debian:bookworm-slim`, `alpine/socat`, `curlimages/curl:8.11.1`.

Nothing captured from a live site is committed. The challenge page is fetched
from the local Anubis at run time, because Anubis embeds the client's IP in it.
