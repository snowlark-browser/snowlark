Snowlark preview build
======================

Snowlark is a fast, private web browser for Windows.
https://snowlark.org

This is an early preview. Many sites work, but expect pages that look wrong, features that are
missing, and crashes. Please do not rely on it for anything important yet.

Build:   {DATE}, revision {SHA}
System:  Windows 10 or Windows 11, 64-bit (x64)


Running it
----------

Unzip the folder anywhere and start snowlark.exe. There is no installer. Keep libEGL.dll and
libGLESv2.dll next to snowlark.exe: WebGL (3D graphics in pages) uses them.

Windows SmartScreen may show "Windows protected your PC" the first time, while a new build earns
its reputation. Choose "More info", then "Run anyway".

Settings, history and bookmarks are at snowlark://settings, snowlark://history and
snowlark://bookmarks. The open source software in Snowlark is listed at snowlark://credits.


Your data
---------

Bookmarks, history, settings, cookies and the cache are kept in

    %LOCALAPPDATA%\Snowlark

To remove Snowlark completely, delete this unzipped folder and that data folder.

Content blocking is on by default. On first start Snowlark downloads its filter lists (EasyList,
EasyPrivacy, uBlock Origin's filters and others) from their publishers, and refreshes them from
time to time. You can change this in snowlark://settings.

Updates: on first start Snowlark asks whether it should check for new versions. Only if you say
yes does it ask github.com, once a day, for the list of Snowlark's releases, download a newer one
and use it the next time it starts. You can change this in snowlark://settings/help.


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
