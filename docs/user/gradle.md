# Gradle

mammoth-cache speaks Gradle's built-in HTTP build cache protocol on `/cache/`, the same path as the
Build Cache Node. Gradle 7.6 through 9.x is supported. The acceptance suite that runs those Gradle
versions against the node in CI ([0008](../design/0008-supported-clients.md)) is still to come.
Older versions back to 4.3 use the same protocol and are expected to work, but won't be tested.

## Configure the build

Add a remote cache to `settings.gradle.kts`. The URL ends in `/cache/`, with the trailing slash:

```kotlin
buildCache {
    remote<HttpBuildCache> {
        url = uri("https://cache.example.com/cache/")
        // Only CI pushes; developers read what CI produced.
        isPush = System.getenv("CI") != null
        // Lets the node turn down an oversized upload before the body is sent.
        isUseExpectContinue = true
    }
}
```

The same in `settings.gradle`:

```groovy
buildCache {
    remote(HttpBuildCache) {
        url = 'https://cache.example.com/cache/'
        push = System.getenv('CI') != null
        useExpectContinue = true
    }
}
```

Then turn the cache on, either per build with `--build-cache` or for every build in
`gradle.properties`:

```properties
org.gradle.caching=true
```

### Plain HTTP

Gradle refuses `http://` URLs unless you allow them. For a node on your own machine or a trusted
network, add:

```kotlin
isAllowInsecureProtocol = true
```

Use HTTPS anywhere credentials are sent. mammoth-cache does not serve TLS itself yet, so for now put
a TLS-terminating proxy or load balancer in front of it.

### Credentials

> [!NOTE]
> Authentication is planned and not built yet. Today every request is allowed, so only run a node
> where everyone who can reach it may read and write the cache.

Once it lands, Gradle sends a user name and a token with HTTP Basic auth, the only scheme Gradle's
client supports. Keep the token out of the build script:

```kotlin
buildCache {
    remote<HttpBuildCache> {
        url = uri("https://cache.example.com/cache/")
        credentials {
            username = "ci"
            password = providers.environmentVariable("MAMMOTH_TOKEN").get()
        }
        isPush = System.getenv("CI") != null
    }
}
```

See [Authentication](authentication.md) for users, tokens and scopes.

## What Gradle sees

Gradle reads only status codes, and turns the remote cache off for the rest of the build after any
unexpected one. mammoth-cache picks its status codes so that a build never fails or loses its cache
because of something the node could have absorbed:

| Request                            | Response                                              |
| ---------------------------------- | ----------------------------------------------------- |
| `GET` a stored key                 | `200` with the entry, `application/octet-stream`      |
| `GET` a missing key                | `404`; Gradle runs the task                           |
| `HEAD` a key                       | `200` with `Content-Length`, or `404`                 |
| `PUT` a new key                    | `201`                                                 |
| `PUT` a key already stored         | `200`; nothing is written, since entries never change |
| `PUT` an entry over the size limit | `200`; the entry is dropped and counted               |
| `PUT` with any `Content-Type`      | Accepted; the body is stored as bytes                 |

Error bodies follow Google's [AIP-193](https://google.aip.dev/193) JSON shape, for people reading
logs or using curl. Gradle ignores them.

## Entry size limit

Entries over 100 MiB, the Build Cache Node's default, are not stored. The upload is answered with
`200` and dropped, because Gradle's client turns the remote cache off after any non-2xx answer to a
store, `413` included, whatever its user guide says. With `isUseExpectContinue = true` and a
`Content-Length` over the limit, the node decides before it reads the body, so Gradle never sends it
and no bytes are wasted. Without it, the body is read and thrown away.

The limit will be set by `limits.max_entry_size` in the [config file](configuration.md). Design doc
[0007](../design/0007-size-limits-and-admission.md) explains why a limit helps the hit rate.

## Check that hits happen

Run the same build twice with a clean between them, and look for `FROM-CACHE` outcomes:

```sh
./gradlew clean build --build-cache --console=plain
```

`--info` logs each remote load and store, and `--scan` shows cache use per task if you use build
scans. On the node, `curl https://cache.example.com/v1/serverStatus` shows hit and miss counts.
