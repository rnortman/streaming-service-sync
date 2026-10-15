# Setting up the Spotify app

`sss` talks to Spotify as an app you register yourself, in Spotify's
"development mode". One app serves the whole family: up to five Spotify
accounts can use it. The steps below are done once, in a browser, by the
person who runs `sss`.

## Before you start

- The account that registers the app must hold **Spotify Premium**. If
  Premium lapses, Spotify stops the app and every `sss` command that talks
  to Spotify fails with an access error until it is renewed.
- Have the Spotify login email of each family member whose account will be
  synced.

## 1. Register the app

1. Open <https://developer.spotify.com/dashboard> and log in with the
   Premium account.
2. Choose **Create app**. Any name and description will do (for example
   "streaming-service-sync").
3. Under **Redirect URIs**, add exactly:

   ```
   https://127.0.0.1/callback
   ```

   No port, no trailing slash. Nothing runs at that address: after a
   family member approves the app, the browser is sent there, fails to load
   the page, and the address it shows is what `sss` needs.
4. Under the APIs used, tick **Web API**.
5. Accept the terms and save.

## 2. Add each family member

Open the app's **Settings**, then **User Management**, and add each
family member's name and Spotify login email, the Premium account's owner
included. A member who is not on this list is refused by Spotify; `sss`
reports that as a configuration error naming this document.

## 3. Give `sss` the client ID

The app's **Settings** page shows its **Client ID** (not the client
secret; `sss` never needs the secret). Put it in the data repository's
`config.toml`:

```toml
[spotify]
client_id = "your-client-id"
```

The client ID is not a secret, but it identifies your app; keep it out of
anything you publish. A profile can use a different app with a
`[profiles.NAME] client_id = "…"` table, for a family of more than five.

## 4. Sign each member in

For each profile, in two steps:

```
sss spotify auth --profile NAME
```

prints an address. Open it in a browser where the family member is logged
in to Spotify (a private window works well when several members share a
computer) and approve. The browser lands on
`https://127.0.0.1/callback?code=…&state=…` and shows an error such as
"can't connect" — that is expected. Copy the whole address from the
address bar, then run

```
sss spotify auth --callback --profile NAME
```

and paste it, followed by an empty line (or save it in a file and add
`--from-file FILE`). The token is stored in the profile's state directory,
readable only by you, and renewed automatically from then on. Run
`sss status` to check it.

If the member declines, or the address belongs to an older attempt, start
again with `sss spotify auth --profile NAME`.
