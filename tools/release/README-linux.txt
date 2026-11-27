Snowlark preview build for Linux
================================

Snowlark is a fast, private web browser.
https://snowlark.org

This is an early preview, and the Linux build is younger than the Windows one. Many sites work,
but expect pages that look wrong, features that are missing, and crashes. Please do not rely on it
for anything important yet.

Build:   {DATE}, revision {SHA}
System:  Linux, 64-bit (x86-64), on a Wayland desktop (GNOME, KDE Plasma, Sway and others;
         X11 desktops through XWayland are not supported yet)
Needs:   the system's fontconfig, FreeType, HarfBuzz, xkbcommon, Wayland client, EGL and D-Bus
         libraries
         (installed on every desktop distribution)


Running it
----------

Unpack the archive anywhere and start ./snowlark in the folder it makes. There is no installer.
Keep libEGL.so and libGLESv2.so next to snowlark: WebGL (3D graphics in pages) uses them, drawn
through the system's OpenGL.

Settings, history and bookmarks are at snowlark://settings, snowlark://history and
snowlark://bookmarks. The open source software in Snowlark is listed at snowlark://credits.

Not there yet on Linux: MPEG audio streams through Media Source Extensions (MP4 and WebM streams
play), dragging out of the window, updates. Video and audio play through the system's FFmpeg (6.1, 7.1 or 8.0)
and sound server; what plays depends on the codecs your distribution's FFmpeg includes. File
dialogs (Save As, the downloads folder) are the desktop's, through its portal (or zenity or
kdialog).
Passwords are saved only where the desktop runs a keyring (the Secret Service: GNOME Keyring,
KWallet, KeePassXC); without one, Snowlark says so and saves none.


Your data
---------

Bookmarks, history, settings, cookies and the cache are kept in

    ~/.config/snowlark    (or $XDG_CONFIG_HOME/snowlark)

To remove Snowlark completely, delete this unpacked folder and that data folder.

Content blocking is on by default. On first start Snowlark downloads its filter lists (EasyList,
EasyPrivacy, uBlock Origin's filters and others) from their publishers, and refreshes them from
time to time. You can change this in snowlark://settings.

This build does not check for new versions: download a new one to update.


Licences and source code
------------------------

Snowlark's own source code is under the Mozilla Public License 2.0:

    https://github.com/snowlark-browser/snowlark

THIRD-PARTY-NOTICES.txt, next to this file, lists the open source software Snowlark contains and
the full text of each licence.

Some of it is under the Mozilla Public License 2.0. Stylo, the CSS engine, is included with
changes; its complete source and the list of changes are at

    https://github.com/snowlark-browser/third-party/tree/main/stylo

The other MPL 2.0 components are included unmodified; THIRD-PARTY-NOTICES.txt says where the
source of each one is.
