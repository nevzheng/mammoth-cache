# Maven

> [!NOTE]
> The `/maven/` routes are served today, from memory and without authentication. The acceptance
> suite that runs real Maven builds against the node in CI is still to come.

mammoth-cache supports the
[Apache Maven Build Cache Extension](https://maven.apache.org/extensions/maven-build-cache-extension/),
version 1.2.0 and later, on Maven 3.9.x. Maven 4 is added once it is generally available. The
Develocity Maven extension is not supported: it needs a Develocity licence and server.

## Configure the build

Add the extension in `.mvn/extensions.xml`:

```xml
<extensions>
  <extension>
    <groupId>org.apache.maven.extensions</groupId>
    <artifactId>maven-build-cache-extension</artifactId>
    <version>1.2.0</version>
  </extension>
</extensions>
```

Point its remote cache at the node's `/maven` path in `.mvn/maven-build-cache-config.xml`:

```xml
<cache xmlns="http://maven.apache.org/BUILD-CACHE-CONFIG/1.0.0">
  <configuration>
    <enabled>true</enabled>
    <remote enabled="true" saveToRemote="false" id="mammoth">
      <url>https://cache.example.com/maven</url>
    </remote>
  </configuration>
</cache>
```

`saveToRemote="false"` makes builds read-only. Let CI push by overriding it on the command line:

```sh
mvn -Dmaven.build.cache.remote.save.enabled=true verify
```

## Credentials

Once [authentication](authentication.md) lands, credentials go in `~/.m2/settings.xml`, under a
`<server>` whose `id` matches the `id` on `<remote>`:

```xml
<settings>
  <servers>
    <server>
      <id>mammoth</id>
      <username>ci</username>
      <password>${env.MAMMOTH_TOKEN}</password>
    </server>
  </servers>
</settings>
```

Maven sends credentials on a `GET` only after the server challenges it with `401` and
`WWW-Authenticate`, and mammoth-cache always sends that header, so no extra setting is needed. To
skip the round trip, set `-Daether.connector.http.preemptiveAuth=true`.

## How it differs from Gradle

- **Paths, not keys.** The extension stores several files per module under paths such as
  `v1.1/<groupId>/<artifactId>/<checksum>/buildinfo.xml`. The node treats each path as an opaque
  key, kept apart from Gradle's keys.
- **Overwrites are allowed.** These files are not content-addressed, so a store replaces what is
  at the path. Gradle entries never change.
- **Oversized stores get `413`.** Maven only logs a failed store and keeps using the cache, so the
  node tells the truth instead of dropping the entry silently. A silent drop could leave a
  `buildinfo.xml` without the files it lists.
- **WebDAV.** With WebDAV on, the extension sends `OPTIONS` first. The node answers without a `DAV`
  header, so the extension skips `MKCOL`. A stray `MKCOL` gets `201` and does nothing.
- **Errors are misses.** Any failed load is treated as a miss, and a failed store never fails the
  build, unless you set `failFast`.
