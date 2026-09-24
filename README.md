# static-file-host

A small, fast file host. Uploading needs a token. Anyone can view a file at its random URL. Files are never deleted, and there is no web UI.

```sh
cargo build --release
./target/release/static-file-host            # first start creates data/token and prints it
./target/release/static-file-host token      # print the token again
```

## Upload

```sh
curl -H "Authorization: Bearer $TOKEN" -F file=@photo.png http://localhost:8080/
# http://localhost:8080/k3j9x0qa7b2m.png

curl -H "Authorization: Bearer $TOKEN" -T Dockerfile http://localhost:8080/
# http://localhost:8080/p0c8w1zz4hq5.Dockerfile
```

- The URL keeps the upload's extension: `a.b.c` becomes `<id>.c`. A name with no dot is used whole: `Dockerfile` becomes `<id>.Dockerfile`. Only the id is used to find the file, so `<id>` without the extension works too.
- The content type comes from the upload. When the upload has none, or sends `application/octet-stream`, the type is detected from the file's first bytes and its extension. UTF-8 text is served with `charset=utf-8`.
- Uploaded URLs use `X-Forwarded-Proto` and `X-Forwarded-Host` (or `Host`). Pass `--public-url https://files.example.com` to set the base yourself.
- To change the token, write a new one to `data/token` and restart.

## Serving

- Responses are sent with `Cache-Control: immutable` and an ETag. Range requests are supported, so video seeking works.
- Crawlers get a `robots.txt` that disallows everything and sets AI content signals. Every response carries an `X-Robots-Tag: noindex, …` header. Requests from known crawler user agents get a 403. Chat link previews (Slack, Discord, Telegram, WhatsApp, X, …) are still allowed so shared links unfurl.

## systemd

```sh
static-file-host systemd --listen 0.0.0.0:8080 | sudo tee /etc/systemd/system/static-file-host.service
sudo systemctl enable --now static-file-host
```

The unit runs as the current user, with the current directory as its working directory.
