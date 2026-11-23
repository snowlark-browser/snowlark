# Password manager

Saving and filling passwords: what each process does, what crosses between them, how the store is kept on
disk, what pages can observe, and the stages it is built in.

## Pieces and where they live

| Piece | Where | Why there |
|---|---|---|
| Field extraction | the page (`snowlark-page`, `passwords` and the forms engine) | only the page has the DOM; it reports raw field facts, never decisions |
| Form parser (which field is the username, current, new and confirmation password) | `snowlark-page::passwords::parse`, a pure module with no engine state, **called in the browser process** | the renderer may be compromised: decisions are made on the browser side, from the facts the page sent |
| Pending save, success detection, save / update / never | `snowlark-shell` (`passwords` and the browser's tab handling) | needs the store and the browser's own knowledge of what each tab committed |
| Store and encryption | `snowlark-shell::passwords` (data), the OS key calls in `snowlark-paint::os_crypt` | the browser process owns the profile; `unsafe` OS calls stay in a crate allowed to hold them |
| Filling | the browser decides, the page applies (`TabCommand`) | the browser picks the credential; the page only puts it in the named fields |
| `snowlark://passwords` | `snowlark-shell` (an internal page, like downloads and history) | browser UI |
| Reauthentication before reveal | `snowlark-paint::window::verify_user` (OS prompt), asked by the shell | an OS call on the browser's window |

### Extraction in the page

The page describes a form as a list of fields: for each text-like `input` (text, email, tel, url, search,
password, number and inputs of no or an unknown type) its renderer id (`NodeId::to_bits`), `type`, `name`,
`id`, `autocomplete`, whether it is focusable and read-only, whether the user typed in it (set by the user
edit path, never by script), and, on submission only, its value. A form is a `form` element and the fields
it owns, or, for fields outside any form, one form per document. The description has the form's action URL
and the frame's URL as the page sees them; the browser does not trust either for any decision about which
site a credential belongs to.

The page sends two things, and only in a browsing context that is the tab's top-level document (frames are
for a later stage):

- **Submitted**: when HTML's form submission algorithm passes the `submit` event and reaches navigation (a
  submission whose event was cancelled sends nothing, as nothing was submitted), the form with values.
- **Rendered**: after the `load` event of a document, the forms that contain a password field, without
  values (an empty list too: the page after a login usually has none). Forms that script adds after
  `load` are reported from stage S2 on.

### Parsing (browser side)

The parser follows the engine family's established heuristics, in this order, each step filling only what the
earlier ones left:

1. **Field facts.** Text-like inputs only. When saving, fields with an empty value, or whose value is one
   repeated non-alphanumeric character (a page's mask), are dropped. A field's *interactability* is
   *certain* when the user typed in it, *possible* when it is focusable, else *unlikely*.
2. **`autocomplete`.** The last token of the attribute (a trailing `webauthn` token is set aside) names the
   field: `username`, `current-password`, `new-password` (the first one is the new password, the second its
   confirmation; when saving, two with different values are ignored), `one-time-code` and `cc-*` mark a field
   that is not a password. `*-password` counts only on password fields and `username` only on others; more
   than one `username` field is ignored. A field given two roles empties the result of this step.
3. **Structure.** Password fields with the best interactability among the likely ones (not read-only unless
   typed in, not named or id'd like a card security code, a social security number or a one-time code),
   non-empty when saving. One field is the current password; two equal non-empty ones are a new password
   and its confirmation, two others current then new; of three, the duplicated pair decides. The username is
   the closest text field before the first password field with the best interactability among those fields,
   preferring a focusable one; when saving, a value of one or two digits is not a username.

The result is a *parsed form*: the field ids and values of username, current password, new password and
confirmation, or nothing when no password field was found.

### Saving

1. The tab's page sends Submitted. The browser takes the **origin from its own record of what the tab's
   top-level document committed** (the session history entry and the process's site lock), never from the
   message, and parses the form in saving mode. A form with a password value becomes the tab's *pending
   credential*: origin, username and its field name, the password (the new password when there is one) and
   its field name, and the form's field names to recognise the form later.
2. The tab navigates. When the next document reports Rendered, the browser compares: if the pending form is
   on the new page again (the same field names in a form with a password field), the login failed and the
   pending credential is dropped (from S2: likewise when the navigation's response was an HTTP error).
   Otherwise the login is taken to have succeeded.
3. On success the browser looks the origin up in the store:
   - the origin is on the *never* list: nothing;
   - the same username with the same password is saved: its last-used time is updated, no prompt;
   - the same username with another password: an **update** prompt;
   - otherwise: a **save** prompt (Save, Never for this site, the close button dismisses).
4. A pending credential lives until the tab commits a second document after it, or the tab closes. A
   submission over HTTP from a page whose origin had HTTPS on the same host is not offered (the downgrade
   could be a network attacker's).

Submissions by script that do not navigate (a fetch followed by the form being removed or hidden) are
recognised from the form disappearing after a submission without navigation; that is a later stage.

### Filling

1. The page sends Rendered. The browser parses each form in filling mode and looks up saved logins whose
   origin is the tab's committed top-level origin (exactly the same scheme, host and port in this stage).
2. It fills on load only when all of these hold: the parsed form has a current-password field (a sign-up
   form with only new-password fields is not filled), the password field is empty, the origin is secure
   (HTTPS, or a local host), exactly one login matches (several wait for the user's choice, a later stage),
   the window is not private. Otherwise nothing is filled.
3. It sends `TabCommand::FillPassword` with the origin it decided for, the field ids, the username and the
   password. The page drops the command unless its top-level document's origin is that origin, and unless
   the named ids are still text inputs (the password one of type password) in that document.
4. **Values are hidden from script until the user interacts.** The page stores the credential as the
   fields' *suggested value*: the field shows it (a password field shows its mask), but `value`, form
   submission and every other script-visible read still see the field's own value. At the first user
   activation in the page (a key press or a click), the suggested values become the fields' values as if the
   user had typed them (the dirty value flag set, `input` and `change` fired). A field the user typed in
   before the fill arrived is not filled. Script that changes a filled field's value before the user
   interacts clears its suggested value.

## What crosses processes, and why a compromised renderer gains nothing

| Message | Direction | Contents |
|---|---|---|
| `TabEvent::PasswordFormSubmitted` | renderer → browser | one form's field facts with values |
| `TabEvent::PasswordFormsRendered` | renderer → browser | forms' field facts, no values |
| `TabCommand::FillPassword` | browser → renderer | origin, two field ids, username, password |

- A renderer only ever receives credentials for the origin the browser itself recorded as the tab's
  committed top-level document, and only when the renderer's site lock is that origin's site. A renderer of
  another site cannot ask for them: there is no request message, the browser decides on its own record.
- A lying renderer can at most make the browser offer to save a credential for its own site, or decline to
  report a form. Both were already within its reach: it runs that site's script.
- The browser parses: a renderer cannot mark fields to make the browser fill a password into a visible text
  field of its choice beyond what its own site's page could do anyway.
- Rendered and Submitted from a renderer that is not the tab's top-level page's (a view, an internal page)
  are refused by the event checks that already guard every tab event; a malformed message kills the renderer.

## Storage on Windows

`passwords.json` in the profile folder, written like the other stores (a temporary file, then a rename):

- `key`: a random 256-bit key, wrapped with the user-scope Data Protection API (`CryptProtectData`) and
  stored as `"DPAPI"` followed by the wrapped bytes, base64. Only the same Windows user on the same machine
  can unwrap it. It is made on first save and never leaves the browser process.
- `logins`: per saved login, the origin, the username and its field name, the password field name, the
  password encrypted, created / last-used times and a use count.
- `never`: the origins the user chose Never for.

A password is encrypted with AES-256-GCM (`ring`, already in the build): `"v10"`, a random 96-bit nonce,
then the ciphertext and tag, base64. The origin is the associated data, so an entry moved to another site in
the file no longer decrypts. Usernames are not encrypted (they are shown in the list without
reauthentication). A store that does not decrypt (another user's or machine's copy) keeps its entries but
offers none of them for filling; the passwords page says they cannot be read.

The OS calls (`CryptProtectData` and `CryptUnprotectData` in `os_crypt`, the reauthentication prompt in
`window`) are in `snowlark-paint`, the crate that already holds the browser window's other OS calls (folder picker,
downloads folder) and is one of those allowed `unsafe`.

Not done: app-bound encryption (a key that only the signed browser executable can unwrap, through a
privileged service) needs an installer and a service; it is noted for when there is an installer.

## Reauthentication

Revealing a password on `snowlark://passwords` asks the OS to confirm the user first: Windows Hello
(`UserConsentVerifier`, face, fingerprint or PIN), in a prompt owned by the browser window. A success is
remembered for 60 seconds, so revealing several passwords asks once. Deleting needs no reauthentication.
The browser never sees what the user entered. Where Windows Hello is not set up, passwords cannot be
revealed in S1; S2 adds the Windows sign-in prompt for the current user as the fallback.

## What pages can observe

- Nothing in `--wpt`, `--render` and `--eval`: the page does not extract forms unless the embedder turns it on
  (`PageOptions::password_forms`), and only the window does. `SNOWLARK_PASSWORD_FORMS=1` turns extraction on
  in the headless modes, to prove a WPT run is the same with it on.
- No new globals, attributes, events or nodes. A filled value is invisible to script until the user
  activates the page, after which the page sees what a user typing it would have produced.
- The fields of a filled form are not styled differently in this stage (the `:autofill` pseudo-class comes
  with the fill choice UI).

## Stages

- **S1** Detect login and sign-up forms; offer save, update and never after a successful submission; the
  encrypted store; fill same-origin saved logins on load; `snowlark://passwords` (list, search, delete,
  reveal behind OS reauthentication); "Passwords" in Delete browsing data.
- **S2** Forms that script adds after `load`; an HTTP error after a submission counts as a failed login;
  the Windows sign-in prompt where Windows Hello is not set up; an "Offer to save passwords" setting;
  import and export CSV (the common `name,url,username,password,note` columns; export behind
  reauthentication); a choice under the field when several logins match, and for related domains (the same
  registrable domain, filled only on the user's click); `navigator.credentials.get/store` for password
  credentials backed by the store (with the user's mediation); submissions by script without navigation;
  frames (same-origin with the top-level document first).
- **S3** Password generation for `new-password` fields (a strong random password offered in the choice
  under the field, saved when the form submits).
- **S4** Checking saved passwords against breach lists, and passkeys (a platform authenticator for
  WebAuthn). Both need decisions first: which breach service, if any, and whether passkeys use Windows
  Hello's own authenticator or a store of the browser's.
