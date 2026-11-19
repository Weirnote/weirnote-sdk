# DoltLite amalgamation

The SDK builds DoltLite from the portable C amalgamation in this directory.
The inputs came from the Weirnote source snapshot
`eb80ad45c56aeba3205a281c371c75b356986619`, with DoltLite source tree
`f3d7f045353fc29b3e9fe6c1c78021e2e8c66a18`. Weirnote includes changes to the
upstream [DoltLite](https://github.com/dolthub/doltlite) engine in this snapshot.
The build uses the DoltLite version label `v0.50.2`.

The existing Weirnote amalgamation script runs `configure` in a scratch build
directory and uses `make -j2 DOLTLITE_VERSION=v0.50.2 sqlite3.c sqlite3.h`.
The checked-in outputs let the SDK build from this repository on either macOS
architecture with the same native source.

| File | SHA-256 |
| --- | --- |
| `sqlite3.c` | `a59eb6272a8203f24db634cc81ca471d0af4acb7b5b64546995be709ddef5525` |
| `sqlite3.h` | `aebd8f6c5de917c95dd12f7428f756ff1be19e2b3f57b63d59733d98298fe677` |

See [DoltLite and SQLite licensing](licenses/LICENSE.md) and the component
notices under [licenses](licenses/). These files accompany the native library
in every wheel.
