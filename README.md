# Walk Relay

**Keeps your group's Big Walk saves in sync so anyone can host.**

Big Walk only saves a journey on the host's computer, and only the host can continue it. Walk Relay is a small tray app that copies every journey to everyone in your group automatically. If the usual host can't make it, anyone else can launch Big Walk and host the latest progress.

Works on **Windows and Mac** (Steam versions of Big Walk).

---

## How it works

You don't press anything. Just play.

1. Someone hosts a journey in Big Walk as usual.
2. When they close Big Walk, Walk Relay uploads that journey.
3. Everyone else's copy updates within a few minutes (about a minute if they were in the session), even if their computer was just sitting there.
4. Next time, whoever is around hosts. The journey continues exactly where it left off.

Safety rules built in:

- **Saves are never touched while Big Walk is open.** Updates wait until you close the game.
- **Old copies can't overwrite newer progress.** The storage server rejects any upload that wasn't based on the latest version.
- **If two people host the same journey separately**, the newer session becomes the current one. The other copy is kept, and you can restore it from **Versions**.
- **Who's hosting is visible.** You get a notification when a friend starts hosting, and a warning if you launch Big Walk while someone is already hosting.
- **Everything is backed up.** The last 20 versions of every journey are stored online, and Walk Relay keeps a local backup before it changes anything.

## Install (everyone)

Download the latest release from this repo's **Releases** page.

**Windows:** run `Walk.Relay_x.y.z_x64-setup.exe`. Windows SmartScreen will say it "protected your PC", because the app isn't code-signed. Click **More info → Run anyway**.

**Mac:** open the `.dmg` and drag **Walk Relay** into **Applications**. The first time you open it, macOS will refuse because the app isn't from the App Store or notarized:
1. Try to open Walk Relay once, and click **Done** on the warning.
2. Open **System Settings → Privacy & Security**, scroll down, and click **Open Anyway** next to Walk Relay. Confirm with your password.
3. If macOS instead says the app "is damaged", open Terminal and run:
   ```bash
   xattr -cr "/Applications/Walk Relay.app"
   ```

When it opens, click the **invite link** a friend sent you (or paste it into the setup screen) and enter **your name**. Each invite works once, so ask for your own. That's it. Walk Relay now starts with your computer and lives in the system tray (Windows) or the menu bar (Mac). Closing the window keeps it running.

## One-time setup (one person does this)

The group needs one small storage bucket. Cloudflare R2's free tier is plenty: a few MB of saves, far under the 10 GB free limit.

1. Create a free account at <https://dash.cloudflare.com> and open **R2 Object Storage**. Cloudflare asks for a card to enable R2, but staying within the free tier costs nothing.
2. **Create bucket**, named e.g. `big-walk`. Keep the defaults.
3. Go back to R2 → **Manage API tokens** → **Create API token**:
   - Permissions: **Object Read & Write**
   - Specify bucket: **only `big-walk`**
4. Copy three values from the result page:
   - **Access Key ID**
   - **Secret Access Key**
   - the **S3 endpoint**: `https://<your-account-id>.r2.cloudflarestorage.com`
5. Install Walk Relay. On the setup screen, open **"First one here? Create the group code"**, paste the values plus the bucket name, and click **Test and create code**. Then click **Use this group for me too**, type your name, and join.
6. **Secure the group** (next section) before inviting anyone.

## Secure your group (recommended, about 10 minutes, once)

A plain group code contains your bucket's key. Anyone who gets hold of an invite, even an old one, could change or delete your saves. Securing the group puts a tiny **Cloudflare Worker** (free) in front of the bucket:

- The key stays inside Cloudflare. Each friend gets their **own pass**, stored only on their computer.
- Invites are **short, single-use and expire after 48 hours**, like `https://walkrelay.<you>.workers.dev/j/7K2Q9XPM`.
- The owner can **remove** anyone at any time (Settings & help → Members).
- Only Walk Relay's own save files are allowed, size-capped, and history can't be deleted.

Steps (the owner does this once):

1. **Pick your workers.dev name.** In the Cloudflare dashboard, open **Workers & Pages**. If it's your first time, it asks you to choose a subdomain; any name works.
2. **Make an API token.** Go to **My Profile → API Tokens → Create Token**, use the **"Edit Cloudflare Workers"** template, set **Account Resources** to your account, and create it. Copy the token.
3. **Give it to GitHub.** In this repo, open **Settings → Secrets and variables → Actions** and add two secrets:
   - `CLOUDFLARE_API_TOKEN`: the token from step 2.
   - `CLOUDFLARE_ACCOUNT_ID`: your account ID, the 32 characters before `.r2.cloudflarestorage.com` in your R2 endpoint.
4. **Deploy.** Open **Actions → Deploy Worker → Run workflow**, enter your bucket name, and run it. The run's summary shows your Worker address.
5. **Secure.** In Walk Relay, open **Settings & help → Secure this group**, paste the Worker address, and click **Secure group**. You're now the owner.
6. **Invite everyone again.** Click **Create invite for Discord** once per friend and DM each their own invite. They click it, type their name, and they're in. Their saves stay where they are; it's the same bucket.
7. **Delete the old R2 API token** (R2 → Manage API tokens). Every old `WR1-`/`WR2-` code and link stops working, and only the Worker can reach the bucket.

If the deploy fails mentioning R2, edit the API token and add **Account → Workers R2 Storage → Read**.

How invites work: clicking an invite opens a page served by your Worker, which opens Walk Relay with the invite filled in, or offers the download if the friend doesn't have it yet. The invite can be used once; after that the link is useless.

## Staying in Cloudflare's free tier

R2 has no spending cap, so Walk Relay keeps its own usage low:

- It checks the bucket every **30 seconds** while you or a friend are playing, and for 10 minutes after a session. Otherwise it checks every **5 minutes**. A small group stays far below the free allowance.
- It counts its bucket operations, and each member shares their count with the group. **Settings & help** shows the group's estimated usage this month.
- If the group passes **70%** of the free monthly allowance, everyone gets a notification and a notice in the window. Syncing carries on; going over costs a few cents per extra million operations.

For extra peace of mind, set a **billing budget alert** in Cloudflare (**Notifications → Billing budget alert**) at $1.

## Updates

Walk Relay checks for a new version when it starts and once a day. When one is out, you get a notification and an **Update now** button at the top of the window. It installs in a few seconds and restarts itself. It never updates while Big Walk is open. You can also click **Settings & help → Check for updates**.

(Version 0.1.0 had no updater, so anyone on it needs to install 0.2.0 by hand once.)

## Tips

- **Don't host the same journey at the same time as someone else.** Walk Relay warns you, and if it happens anyway it keeps both versions. But you'd have two diverging journeys.
- **Steam Cloud** can stay on. Walk Relay only changes save files while Big Walk is closed.
- **Console players** (PS5, Switch 2) can join your sessions, but their own saves stay on their console and can't be synced.
- Walk Relay only syncs Big Walk journey files (`save_<name>_<id>.sav` and `.sav.bak`) and never touches anything else in the folder.

## Where things live

| | Windows | Mac |
|---|---|---|
| Big Walk saves | `%USERPROFILE%\AppData\LocalLow\House House\Big Walk\user_data\save_games` | `~/Library/Application Support/House House/Big Walk/user_data/save_games` (and `…/unity.House House.Big Walk/…` if that exists; both are watched) |
| Walk Relay settings & local backups | `%APPDATA%\Walk Relay` | `~/Library/Application Support/Walk Relay` |

## Troubleshooting

- **"Big Walk save folder not found":** launch Big Walk once so it creates its folder.
- **"Can't sync right now":** usually no internet connection. It retries on its own every 30 seconds.
- **I want an older version back:** open Walk Relay, click **Versions** next to the journey, then **Restore**. Everyone gets it once their Big Walk is closed.
- **Something went really wrong:** every file Walk Relay replaced is in the local backups folder (**Settings & help → Open local backups**).

---

## For developers

```
core/        sync engine (pure Rust, no GUI): paths, game detection, bundles, R2 client, engine
  tests/     engine.rs: multi-friend scenarios on an in-memory bucket
             s3_live.rs: the real S3 client against a live bucket (set WALKRELAY_TEST_CODE)
worker/      Cloudflare Worker for secured groups (npm test runs its tests on Miniflare)
src-tauri/   Tauri v2 tray app (main.rs)
ui/          the window (plain HTML/CSS/JS)
```

```bash
cargo test -p walkrelay-core          # engine tests
npm install && npx tauri dev          # run the app locally
```

Remote layout in the bucket (`v1/`): `journeys/<hex stem>.zip` holds the current version of each journey (the `.sav` and `.sav.bak` plus a manifest). `history/` and `conflicts/` hold earlier and set-aside versions. `presence/<hex name>.json` records who is playing or hosting.

**Releases:** in GitHub, open **Actions → Release → Run workflow**. That builds the Windows installer and the Mac app and publishes them as a release tagged with the version in `src-tauri/tauri.conf.json`. Bump that version, the workspace version in `Cargo.toml` and `package.json` for each new release.

Releases are signed for the in-app updater. The workflow needs the repository secrets `TAURI_SIGNING_PRIVATE_KEY` and `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, and the matching public key is in `tauri.conf.json` (`plugins.updater.pubkey`). The release also publishes `latest.json`, which installed copies check for updates. **Keep a backup of the private key**: without it, installed copies can't be updated automatically, and everyone would have to reinstall by hand.

There's also a headless CLI: `walkrelay-cli make-code | check | sync`.
