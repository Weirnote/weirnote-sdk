# Apple sign-in and sync on macOS

Install the Weirnote SDK with pip or uv. The wheel contains the small Weirnote Python Account helper. On first account use, the SDK verifies and sets up the helper in your user Library. The SDK opens the browser when you explicitly sign in to a new profile.

```python
from weirnote import Client

client = Client(profile="scripts")
print(client.sign_in_with_apple())
```

Virtual environments for the same macOS user share the helper and named profiles. The helper stores device keys in macOS Keychain. Calling `identity()` reads the saved identity locally and returns `None` before enrollment. Authenticated operations renew the session as needed. Calling `sign_out()` removes the local profile key and leaves your libraries on disk.

Use `helper_path=` to run a specific test bundle. Local source builds use ad-hoc signing and a development bundle identifier. To exercise browser sign-in with a local build, use a test service that allows its development callback.

See the [Python API reference](python-api.md#client-and-libraries) for cloning, bootstrapping, and synchronizing an account library.
