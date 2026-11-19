// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `snowlark://bookmarks`: the bookmark manager. The folders in a tree, the selected folder's items
//! (or a search's) in a list, and the changes: edit, add, add a folder, delete (with undo), sort,
//! and drag to reorder or move between folders. The page is made here from the bookmarks; its
//! script sends each change to `snowlark://bookmarks/apply`, which the browser applies before
//! showing the page again.

use snowlark_net::Url;

use crate::bookmarks::{BookmarkId, Bookmarks, Item, Place};
use crate::history_page::{colors, encode};
use crate::settings::{escape_html, form_pairs};

pub const BOOKMARKS_PAGE: &str = "snowlark://bookmarks/";

/// How long the "deleted" notice with its Undo button stays (milliseconds).
pub const TOAST_MS: i64 = 10_000;

const SCRIPT: &str = include_str!("bookmarks_page.js");

pub fn is_bookmarks_url(url: &Url) -> bool {
    url.scheme() == "snowlark" && url.host_str() == Some("bookmarks")
}

/// What the address asks the page to show: a folder (`?id=`; none: the bar) or a search (`?q=`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub folder: Option<BookmarkId>,
    pub query: String,
}

impl View {
    pub fn of(url: &Url) -> View {
        let pairs = form_pairs(url.query().unwrap_or(""));
        let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        View {
            folder: get("id").and_then(parse_folder),
            query: get("q").unwrap_or("").trim().to_owned(),
        }
    }

    /// The page's address for this view.
    pub fn url(&self) -> String {
        if !self.query.is_empty() {
            format!("{BOOKMARKS_PAGE}?q={}", encode(&self.query))
        } else if let Some(id) = self.folder {
            format!("{BOOKMARKS_PAGE}?id={id}")
        } else {
            BOOKMARKS_PAGE.to_owned()
        }
    }
}

/// A folder in an address or a form: `0` (or nothing) is the bar.
fn parse_folder(s: &str) -> Option<BookmarkId> {
    s.parse().ok().filter(|&id| id != 0)
}

/// The address the page is shown at.
pub fn shown_url(url: &Url) -> Url {
    Url::parse(&View::of(url).url()).unwrap_or_else(|_| url.clone())
}

/// A change the page's script asks for (`snowlark://bookmarks/apply?op=...`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// A new name and, for a page, address.
    Edit {
        id: BookmarkId,
        title: String,
        url: Option<String>,
    },
    /// A new page (`url`) or folder (`url` `None`) at the end of `parent`.
    Add {
        parent: Option<BookmarkId>,
        title: String,
        url: Option<String>,
    },
    Delete(Vec<BookmarkId>),
    /// The last delete, put back.
    Undo,
    /// Items dropped at `to` (the index counted before they move).
    Move {
        ids: Vec<BookmarkId>,
        to: Place,
    },
    /// A folder sorted by name.
    Sort(Option<BookmarkId>),
    /// The pages (a folder: the pages in it) opened in new tabs; with `active`, the first one
    /// shown.
    Open {
        ids: Vec<BookmarkId>,
        active: bool,
    },
}

impl Op {
    pub fn parse(query: &str) -> Option<Op> {
        let pairs = form_pairs(query);
        let get = |k: &str| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        let ids: Vec<BookmarkId> = pairs
            .iter()
            .filter(|(k, _)| k == "id")
            .filter_map(|(_, v)| v.parse().ok())
            .collect();
        let parent = get("parent").and_then(parse_folder);
        let title = get("title").unwrap_or("").to_owned();
        let url = get("url").map(str::to_owned);
        Some(match get("op")? {
            "edit" => Op::Edit {
                id: *ids.first()?,
                title,
                url,
            },
            "add" => Op::Add {
                parent,
                title,
                url: Some(url?),
            },
            "folder" => Op::Add {
                parent,
                title,
                url: None,
            },
            "delete" => Op::Delete(ids),
            "undo" => Op::Undo,
            "move" => Op::Move {
                ids,
                to: Place {
                    parent,
                    // (none: a drop on a folder, at its end)
                    index: match get("index") {
                        Some(i) => i.parse().ok()?,
                        None => usize::MAX,
                    },
                },
            },
            "sort" => Op::Sort(parent),
            "open" => Op::Open {
                ids,
                active: get("active") == Some("1"),
            },
            _ => return None,
        })
    }
}

/// An address as the edit dialog takes it: as typed when it parses, otherwise with `http://` in
/// front when that parses; `None` when neither does.
pub fn fixed_url(typed: &str) -> Option<String> {
    let typed = typed.trim();
    if typed.is_empty() {
        return None;
    }
    if let Ok(u) = Url::parse(typed) {
        return Some(u.as_str().to_owned());
    }
    Url::parse(&format!("http://{typed}"))
        .ok()
        .map(|u| u.as_str().to_owned())
}

/// The notice after a delete: `'Title' deleted`, or how many went.
pub fn deleted_label(items: &[&Item]) -> String {
    match items {
        [one] => format!("'{}' deleted", one.label()),
        _ => format!("{} bookmarks deleted", items.len()),
    }
}

/// The MIME type of an icon file from its base64 (the bar keeps PNG, ICO and SVG files).
fn icon_type(b64: &str) -> &'static str {
    match b64.get(..4) {
        Some("AAAB") => "image/x-icon",
        Some("PHN2" | "PD94") => "image/svg+xml",
        Some("/9j/") => "image/jpeg",
        Some("R0lG") => "image/gif",
        Some("UklG") => "image/webp",
        _ => "image/png",
    }
}

const FOLDER: &str = "<svg class=folder-icon viewBox='0 0 20 20' aria-hidden=true><path d='M2.5 5.5a1.5 1.5 0 0 1 1.5-1.5h4l2 2h6a1.5 1.5 0 0 1 1.5 1.5v7.5a1.5 1.5 0 0 1-1.5 1.5H4a1.5 1.5 0 0 1-1.5-1.5z'/></svg>";

const GLOBE: &str = "<svg class=globe viewBox='0 0 16 16' aria-hidden=true><circle cx='8' cy='8' r='6.5'/><path d='M1.5 8h13M8 1.5c-2.2 2.2-2.2 10.8 0 13M8 1.5c2.2 2.2 2.2 10.8 0 13'/></svg>";

const STAR: &str = "<svg viewBox='0 0 24 24' aria-hidden=true><path d='M12 3.8l2.5 5.2 5.7.8-4.1 4 1 5.6-5.1-2.7-5.1 2.7 1-5.6-4.1-4 5.7-.8z'/></svg>";

const ARROW: &str = "<svg viewBox='0 0 20 20' aria-hidden=true><path d='M6 8l4 4 4-4z'/></svg>";

const DOTS: &str = "<svg viewBox='0 0 24 24' aria-hidden=true><circle cx='12' cy='5.5' r='1.6'/><circle cx='12' cy='12' r='1.6'/><circle cx='12' cy='18.5' r='1.6'/></svg>";

/// How many pages a folder holds directly (what "Open all" opens).
fn pages_in(item: &Item) -> usize {
    item.children
        .as_deref()
        .map_or(0, |c| c.iter().filter(|i| !i.is_folder()).count())
}

/// The folder tree: the bar, then its folders, each open when it holds the selected folder.
fn tree(b: &Bookmarks, selected: Option<Option<BookmarkId>>) -> String {
    let open_path: Vec<BookmarkId> = {
        let mut path = Vec::new();
        let mut cur = selected.flatten().and_then(|id| b.place_of(id)?.parent);
        while let Some(id) = cur {
            path.push(id);
            cur = b.place_of(id).and_then(|p| p.parent);
        }
        path
    };
    struct Node<'a> {
        id: Option<BookmarkId>,
        title: &'a str,
        children: &'a [Item],
        place: Option<Place>,
        pages: usize,
    }
    fn node(
        out: &mut String,
        n: Node,
        depth: usize,
        selected: Option<Option<BookmarkId>>,
        open_path: &[BookmarkId],
    ) {
        let folders: Vec<&Item> = n.children.iter().filter(|i| i.is_folder()).collect();
        let id = n.id.unwrap_or(0);
        let is_selected = selected == Some(n.id);
        let open = n.id.is_none_or(|i| open_path.contains(&i));
        let (parent, index) = n.place.map_or((String::new(), String::new()), |p| {
            (p.parent.unwrap_or(0).to_string(), p.index.to_string())
        });
        out.push_str(&format!(
            "<div class='node{open}' data-id={id}><div class='folder{sel}' role=treeitem aria-level={level} aria-selected={is_selected}{expanded} \
data-id={id} data-parent='{parent}' data-index='{index}' data-pages={pages} data-title='{title}' style='--depth:{depth}' tabindex=-1>\
{arrow}{FOLDER}<a class=label href='{BOOKMARKS_PAGE}?id={id}' title='{title}' tabindex=-1>{title}</a></div>",
            open = if open { " open" } else { "" },
            sel = if is_selected { " selected" } else { "" },
            level = depth + 1,
            expanded = if folders.is_empty() {
                String::new()
            } else {
                format!(" aria-expanded={open}")
            },
            pages = n.pages,
            title = escape_html(n.title),
            arrow = if folders.is_empty() {
                "<span class=arrow-space></span>".to_owned()
            } else {
                format!("<button type=button class=arrow tabindex=-1 aria-hidden=true>{ARROW}</button>")
            },
        ));
        if !folders.is_empty() {
            out.push_str("<div class=children role=group>");
            for (k, f) in n.children.iter().enumerate() {
                if !f.is_folder() {
                    continue;
                }
                node(
                    out,
                    Node {
                        id: Some(f.id),
                        title: f.label(),
                        children: f.children.as_deref().unwrap_or(&[]),
                        place: Some(Place {
                            parent: n.id,
                            index: k,
                        }),
                        pages: pages_in(f),
                    },
                    depth + 1,
                    selected,
                    open_path,
                );
            }
            out.push_str("</div>");
        }
        out.push_str("</div>");
    }
    let mut out = String::new();
    node(
        &mut out,
        Node {
            id: None,
            title: "Bookmarks bar",
            children: b.bar(),
            place: None,
            pages: b.bar().iter().filter(|i| !i.is_folder()).count(),
        },
        0,
        selected,
        &open_path,
    );
    out
}

/// A row of the list.
fn row(out: &mut String, item: &Item, place: Place) {
    let icon = if item.is_folder() {
        FOLDER.to_owned()
    } else if let Some(b64) = item.icon.as_deref().filter(|s| !s.is_empty()) {
        format!(
            "<img class=favicon src='data:{};base64,{}' alt=''>",
            icon_type(b64),
            escape_html(b64)
        )
    } else {
        GLOBE.to_owned()
    };
    let title = escape_html(item.label());
    out.push_str(&format!(
        "<div class=row role=row tabindex=-1 aria-selected=false data-id={id} data-parent={parent} data-index={index}{folder} \
data-url='{url}' data-title='{plain}'><span class=icon>{icon}</span><span class=text role=gridcell>\
<span class=title title='{title}'>{title}</span><span class=url title='{url}'>{url}</span></span>\
<button type=button class=more title='More actions' aria-label='More actions for {title}' aria-haspopup=menu>{DOTS}</button></div>",
        id = item.id,
        parent = place.parent.unwrap_or(0),
        index = place.index,
        folder = if item.is_folder() {
            format!(" data-folder={}", pages_in(item))
        } else {
            String::new()
        },
        url = escape_html(&item.url),
        plain = escape_html(&item.title),
    ));
}

const STYLE: &str = "\
body{font:14px 'Segoe UI',sans-serif;margin:0;background:var(--bg);color:var(--text)}\
[hidden]{display:none!important}\
header{position:sticky;top:0;z-index:2;display:flex;align-items:center;gap:12px;height:56px;padding:0 8px 0 20px;background:var(--bg);border-bottom:1px solid var(--line)}\
header .brand{display:flex;align-items:center;gap:12px;flex:none;width:236px}\
header .brand svg{width:24px;height:24px;fill:none;stroke:var(--accent);stroke-width:1.8;stroke-linejoin:round}header .name{font:500 20px 'Segoe UI',sans-serif}\
.search-wrap{flex:1;min-width:0;padding:0 24px}\
.search{display:flex;align-items:center;gap:8px;max-width:632px;height:40px;margin:0 auto;padding:0 8px 0 14px;box-sizing:border-box;border-radius:20px;background:var(--field)}\
.search svg{width:20px;height:20px;flex:none;fill:none;stroke:var(--muted);stroke-width:1.8;stroke-linecap:round}\
.search input{flex:1;min-width:0;border:0;background:transparent;color:inherit;font:inherit;outline:none;padding:0}\
.search button{border:0;background:transparent;color:var(--muted);padding:4px 8px;border-radius:12px}.search button:hover{background:var(--hover)}\
button.more{flex:none;width:32px;height:32px;border:0;border-radius:16px;background:transparent;padding:0;cursor:pointer}\
button.more:hover{background:var(--hover)}button.more svg{width:20px;height:20px;fill:var(--muted)}\
#selection{display:none;background:var(--sel);color:var(--sel-text);border-bottom-color:transparent}\
body.selecting #selection{display:flex}body.selecting #toolbar{display:none}\
#selection .count{flex:1;font:500 16px 'Segoe UI',sans-serif}\
#selection button,dialog-box .buttons button,#toast button{border:0;border-radius:18px;padding:8px 16px;font:500 14px 'Segoe UI',sans-serif;background:transparent;color:inherit;cursor:pointer}\
#selection button:hover{background:var(--hover)}\
button.action{background:var(--accent)!important;color:var(--on-accent)!important}\
.layout{display:flex;align-items:flex-start}\
nav{position:sticky;top:57px;flex:none;width:256px;max-height:calc(100vh - 57px);overflow:auto;padding:8px 0;box-sizing:border-box}\
.folder{position:relative;display:flex;align-items:center;gap:4px;height:40px;padding:0 16px 0 calc(8px + var(--depth) * 20px);margin:0 12px 0 0;border-radius:0 20px 20px 0;cursor:pointer;color:var(--text);outline:none}\
.folder:hover{background:var(--hover)}.folder.selected{background:var(--sel);color:var(--sel-text);font-weight:600}\
.folder .label{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;color:inherit;text-decoration:none;margin-left:12px}\
.arrow,.arrow-space{flex:none;width:24px;height:24px}\
.arrow{border:0;border-radius:12px;background:transparent;padding:2px;cursor:pointer;color:inherit}.arrow:hover{background:var(--hover)}\
.arrow svg{width:20px;height:20px;fill:currentColor;transform:rotate(-90deg)}.node.open>.folder .arrow svg{transform:none}\
.node:not(.open)>.children{display:none}\
.folder-icon{width:20px;height:20px;flex:none;fill:none;stroke:currentColor;stroke-width:1.5;stroke-linejoin:round}\
.folder.selected .folder-icon,.row .folder-icon{fill:currentColor;fill-opacity:.25}\
main{flex:1;min-width:0;max-width:960px;margin:0 auto;padding:8px 24px 64px}\
.card{background:var(--card);border-radius:8px;padding:8px 0;margin:16px 0;box-shadow:0 0 0 1px var(--line)}\
.row{position:relative;display:flex;align-items:center;gap:16px;height:40px;padding:0 8px 0 20px;cursor:default;outline:none;user-select:none}\
.row:hover{background:var(--hover)}.row.selected{background:var(--sel);color:var(--sel-text)}\
.row .icon{display:flex;flex:none;width:16px;height:16px;color:var(--muted)}.row .icon svg,.row .icon img{width:16px;height:16px}\
.globe{fill:none;stroke:var(--muted);stroke-width:1.2}\
.row .text{flex:1;min-width:0;display:flex;gap:16px;align-items:baseline}\
.row .title{flex:0 1 auto;max-width:60%;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}\
.row .url{flex:1;min-width:0;color:var(--muted);font-size:13px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}\
.row.selected .url{color:inherit}\
.drag-above::before,.drag-below::after{content:'';position:absolute;left:0;right:0;height:2px;background:var(--accent);pointer-events:none}\
.drag-above::before{top:-1px}.drag-below::after{bottom:-1px}\
.drag-on{box-shadow:inset 0 0 0 2px var(--accent)}.dragged{opacity:.5}.folder{user-select:none}\
#drag-image{position:fixed;z-index:6;pointer-events:none;padding:6px 12px;border-radius:4px;background:var(--card);box-shadow:0 2px 6px #0000004d;max-width:240px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}\
#menu{position:absolute;z-index:3;min-width:200px;padding:8px 0;background:var(--card);border-radius:4px;box-shadow:0 2px 6px #0000004d,0 0 0 1px var(--line)}\
#menu button{display:block;width:100%;text-align:left;border:0;background:transparent;color:var(--text);font:inherit;padding:8px 24px;cursor:pointer}\
#menu button:hover{background:var(--hover)}#menu button:disabled{color:var(--muted);opacity:.6;cursor:default}#menu button:disabled:hover{background:transparent}\
#menu hr{border:0;border-top:1px solid var(--line);margin:8px 0}\
#scrim{position:fixed;inset:0;z-index:4;display:flex;align-items:center;justify-content:center;background:var(--scrim)}\
dialog-box{display:block;width:420px;padding:20px 24px 16px;background:var(--card);border-radius:8px;box-shadow:0 4px 16px #0000004d}\
dialog-box h2{font:500 18px 'Segoe UI',sans-serif;margin:0 0 16px}\
dialog-box label{display:block;margin:0 0 16px;color:var(--muted);font-size:12px}\
dialog-box input{display:block;width:100%;box-sizing:border-box;margin-top:4px;height:36px;padding:0 8px;border:0;border-radius:4px;background:var(--field);color:var(--text);font:14px 'Segoe UI',sans-serif;outline:none;border-bottom:2px solid transparent}\
dialog-box input:focus{border-bottom-color:var(--accent)}dialog-box .invalid input{border-bottom-color:#d93025}\
dialog-box .error{color:#d93025;font-size:12px;margin-top:4px}\
dialog-box .buttons{display:flex;justify-content:flex-end;gap:8px;margin-top:8px}\
dialog-box .buttons button{color:var(--accent);box-shadow:0 0 0 1px var(--line)}\
#toast{position:fixed;left:24px;bottom:24px;z-index:5;display:flex;align-items:center;gap:24px;min-width:288px;max-width:568px;padding:6px 8px 6px 24px;box-sizing:border-box;border-radius:8px;background:var(--text);color:var(--bg)}\
#toast .label{flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}#toast button{color:var(--accent-on-dark)}\
.empty{text-align:center;color:var(--muted);font-size:16px;padding:64px 0}a{color:var(--accent)}\
@media (max-width:820px){header .brand{width:auto}header .name{display:none}nav{width:200px}.row .url{display:none}}";

/// The page for `view` of the bookmarks. `toast` is the delete notice still showing, with the
/// milliseconds it has left.
pub fn bookmarks_html(
    b: &Bookmarks,
    view: &View,
    dark: bool,
    toast: Option<(&str, i64)>,
) -> String {
    let searching = !view.query.is_empty();
    // (an id that names no folder shows the bar)
    let folder = view
        .folder
        .filter(|&id| b.find(id).is_some_and(Item::is_folder));
    let mut rows = String::new();
    let mut count = 0;
    if searching {
        for item in b.search(&view.query) {
            if let Some(place) = b.place_of(item.id) {
                row(&mut rows, item, place);
                count += 1;
            }
        }
    } else {
        let list = match folder {
            Some(id) => b
                .find(id)
                .and_then(|f| f.children.as_deref())
                .unwrap_or(&[]),
            None => b.bar(),
        };
        for (index, item) in list.iter().enumerate() {
            row(
                &mut rows,
                item,
                Place {
                    parent: folder,
                    index,
                },
            );
        }
        count = list.len();
    }
    let list = if count == 0 {
        format!(
            "<div id=list class=empty-list data-folder={}><p class=empty>{}</p></div>",
            folder.unwrap_or(0),
            if searching {
                "No search results found"
            } else {
                "To bookmark pages, click the star in the address bar"
            }
        )
    } else {
        format!(
            "<div id=list class=card role=grid aria-label='Bookmark list' aria-multiselectable=true data-folder={}>{rows}</div>",
            folder.unwrap_or(0)
        )
    };
    let tree = tree(b, (!searching).then_some(folder));
    let title = if searching {
        "Bookmarks".to_owned()
    } else {
        folder
            .and_then(|id| b.find(id))
            .map_or("Bookmarks".to_owned(), |f| f.label().to_owned())
    };
    let toast = toast.filter(|(_, ms)| *ms > 0);
    let (scheme, colors) = colors(dark);
    format!(
        "<!doctype html><html><head><meta charset=utf-8><title>{title}</title><style>\
html{{color-scheme:{scheme};{colors};--accent-on-dark:{on_dark}}}{STYLE}</style></head>\
<body data-searching={searching}>\
<header id=toolbar><div class=brand>{STAR}<span class=name>Bookmarks</span></div>\
<div class=search-wrap><form class=search action='{BOOKMARKS_PAGE}' method=get role=search>\
<svg viewBox='0 0 24 24'><circle cx='10.5' cy='10.5' r='6'/><path d='M15 15l5.5 5.5'/></svg>\
<input id=search name=q type=search value='{query}' placeholder='Search bookmarks' aria-label='Search bookmarks' autocomplete=off spellcheck=false>\
<button id=clear-search type=button aria-label='Clear search'{clear_hidden}>&#x2715;</button></form></div>\
<button id=organize type=button class=more title=Organize aria-label=Organize aria-haspopup=menu>{DOTS}</button></header>\
<header id=selection><button id=cancel-x type=button aria-label=Cancel>&#x2715;</button><span class=count></span>\
<button id=delete type=button class=action>Delete</button></header>\
<div class=layout><nav id=tree role=tree aria-label='Bookmark folder tree'>{tree}</nav><main>{list}</main></div>\
<div id=menu role=menu hidden>\
<button type=button role=menuitem data-command=edit>Edit</button>\
<button type=button role=menuitem data-command=show>Show in folder</button>\
<button type=button role=menuitem data-command=delete>Delete</button>\
<hr data-after=delete>\
<button type=button role=menuitem data-command=open>Open in new tab</button>\
<button type=button role=menuitem data-command=sort>Sort by name</button>\
<hr data-after=sort>\
<button type=button role=menuitem data-command=add>Add new bookmark</button>\
<button type=button role=menuitem data-command=folder>Add new folder</button></div>\
<div id=scrim hidden><dialog-box role=dialog aria-modal=true aria-labelledby=dialog-title><h2 id=dialog-title></h2>\
<label>Name<input id=name autocomplete=off spellcheck=false></label>\
<label id=url-field>URL<input id=url type=url autocomplete=off spellcheck=false><span id=url-error class=error hidden>Invalid URL.</span></label>\
<div class=buttons><button id=dialog-cancel type=button>Cancel</button><button id=dialog-save type=button class=action>Save</button></div></dialog-box></div>\
<div id=toast role=alert data-ms={toast_ms}{toast_hidden}><span class=label>{toast_label}</span><button id=undo type=button aria-label='Undo delete'>Undo</button></div>\
<script>{SCRIPT}</script></body></html>",
        title = escape_html(&title),
        on_dark = if dark { "#0b57d0" } else { "#a8c7fa" },
        query = escape_html(&view.query),
        clear_hidden = if searching { "" } else { " hidden" },
        toast_ms = toast.map_or(0, |(_, ms)| ms),
        toast_hidden = if toast.is_some() { "" } else { " hidden" },
        toast_label = escape_html(toast.map_or("", |(l, _)| l)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn the_address_names_a_folder_or_a_search() {
        assert_eq!(View::of(&url("snowlark://bookmarks/")), View::default());
        assert_eq!(
            View::of(&url("snowlark://bookmarks/?id=0")),
            View::default()
        );
        let v = View::of(&url("snowlark://bookmarks/?id=7"));
        assert_eq!(v.folder, Some(7));
        assert_eq!(v.url(), "snowlark://bookmarks/?id=7");
        let v = View::of(&url("snowlark://bookmarks/?q=a+b%26c"));
        assert_eq!(v.query, "a b&c");
        assert_eq!(v.url(), "snowlark://bookmarks/?q=a+b%26c");
        assert_eq!(
            shown_url(&url("snowlark://bookmarks?id=3&x=1")).as_str(),
            "snowlark://bookmarks/?id=3"
        );
    }

    #[test]
    fn the_changes_parse_from_the_apply_address() {
        let p = Op::parse;
        assert_eq!(
            p("op=edit&id=4&title=A+b&url=https%3A%2F%2Fa.example%2F"),
            Some(Op::Edit {
                id: 4,
                title: "A b".into(),
                url: Some("https://a.example/".into())
            })
        );
        assert_eq!(
            p("op=folder&parent=0&title=New"),
            Some(Op::Add {
                parent: None,
                title: "New".into(),
                url: None
            })
        );
        assert_eq!(p("op=add&parent=3&title=x"), None);
        assert_eq!(p("op=delete&id=1&id=2"), Some(Op::Delete(vec![1, 2])));
        assert_eq!(p("op=undo"), Some(Op::Undo));
        assert_eq!(
            p("op=move&id=5&id=6&parent=2&index=0"),
            Some(Op::Move {
                ids: vec![5, 6],
                to: Place {
                    parent: Some(2),
                    index: 0
                }
            })
        );
        assert_eq!(
            p("op=move&id=5&parent=2"),
            Some(Op::Move {
                ids: vec![5],
                to: Place {
                    parent: Some(2),
                    index: usize::MAX
                }
            })
        );
        assert_eq!(p("op=move&id=5&parent=2&index=x"), None);
        assert_eq!(p("op=sort&parent=9"), Some(Op::Sort(Some(9))));
        assert_eq!(
            p("op=open&id=1&active=1"),
            Some(Op::Open {
                ids: vec![1],
                active: true
            })
        );
        assert_eq!(p("op=nothing"), None);
        assert_eq!(p(""), None);
    }

    #[test]
    fn a_typed_address_gets_a_scheme_when_it_needs_one() {
        assert_eq!(
            fixed_url("example.com/a").as_deref(),
            Some("http://example.com/a")
        );
        assert_eq!(
            fixed_url(" https://a.example ").as_deref(),
            Some("https://a.example/")
        );
        assert_eq!(
            fixed_url("javascript:void(0)").as_deref(),
            Some("javascript:void(0)")
        );
        assert_eq!(fixed_url(""), None);
        assert_eq!(fixed_url("a b"), None);
    }

    fn sample() -> (Bookmarks, BookmarkId, BookmarkId) {
        let mut b = Bookmarks::in_memory();
        let at = |parent, index| Place { parent, index };
        b.add(
            Item::page("Alpha", "https://a.example/", Some("iVBORw0KGgo=".into())),
            at(None, 9),
        );
        let news = b.add(Item::folder("News"), at(None, 9)).unwrap();
        b.add(
            Item::page("", "https://untitled.example/", None),
            at(None, 9),
        );
        b.add(
            Item::page("Paper <b>", "https://news.example/", None),
            at(Some(news), 9),
        );
        let old = b.add(Item::folder("Old"), at(Some(news), 9)).unwrap();
        (b, news, old)
    }

    #[test]
    fn the_page_lists_the_selected_folder_and_shows_the_tree() {
        let (b, news, old) = sample();
        let html = bookmarks_html(&b, &View::default(), false, None);
        assert!(html.contains("<title>Bookmarks</title>"));
        // The bar's items, with their icons and an untitled page by its address.
        assert!(html.contains("data-title='Alpha'"));
        assert!(html.contains("src='data:image/png;base64,iVBORw0KGgo='"));
        assert!(html.contains("<span class=title title='https://untitled.example/'>"));
        assert!(html.contains(&format!(
            "data-id={news} data-parent=0 data-index=1 data-folder=1"
        )));
        assert!(!html.contains("Paper"));
        // The tree: the bar selected, its folders, a folder inside a closed one.
        assert!(html.contains("class='folder selected' role=treeitem aria-level=1"));
        assert!(html.contains(&format!("href='snowlark://bookmarks/?id={old}'")));
        assert!(html.contains(&format!("<div class='node' data-id={news}>")));
        // A folder: its items, its title, the folders above it open.
        let html = bookmarks_html(
            &b,
            &View {
                folder: Some(old),
                query: String::new(),
            },
            false,
            None,
        );
        assert!(html.contains("<title>Old</title>"));
        assert!(html.contains("To bookmark pages, click the star in the address bar"));
        assert!(html.contains(&format!("<div class='node open' data-id={news}>")));
        let html = bookmarks_html(
            &b,
            &View {
                folder: Some(news),
                query: String::new(),
            },
            false,
            None,
        );
        assert!(html.contains("Paper &lt;b&gt;"));
        assert!(!html.contains("data-title='Alpha'"));
        // An id that names no folder shows the bar.
        let html = bookmarks_html(
            &b,
            &View {
                folder: Some(999),
                query: String::new(),
            },
            false,
            None,
        );
        assert!(html.contains("data-title='Alpha'"));
    }

    #[test]
    fn a_search_lists_what_matches_wherever_it_is() {
        let (b, news, _) = sample();
        let view = View {
            folder: None,
            query: "news".into(),
        };
        let html = bookmarks_html(&b, &view, false, None);
        assert!(html.contains(&format!("data-id={news} data-parent=0 data-index=1")));
        assert!(html.contains(&format!(
            "data-parent={news} data-index=0 data-url='https://news.example/'"
        )));
        assert!(!html.contains("data-title='Alpha'"));
        // No folder is selected in the tree while searching.
        assert!(!html.contains("folder selected"));
        let view = View {
            folder: None,
            query: "nothing here".into(),
        };
        assert!(bookmarks_html(&b, &view, false, None).contains("No search results found"));
    }

    #[test]
    fn the_delete_notice_shows_while_it_has_time_left() {
        let (b, _, _) = sample();
        let html = bookmarks_html(&b, &View::default(), false, Some(("'Alpha' deleted", 4000)));
        assert!(html.contains("<div id=toast role=alert data-ms=4000><span class=label>&#39;Alpha&#39; deleted</span>"));
        let html = bookmarks_html(&b, &View::default(), false, Some(("'Alpha' deleted", 0)));
        assert!(html.contains("data-ms=0 hidden>"));
        let a = Item::page("A", "https://a.example/", None);
        let c = Item::page("", "https://c.example/", None);
        assert_eq!(deleted_label(&[&a]), "'A' deleted");
        assert_eq!(deleted_label(&[&c]), "'https://c.example/' deleted");
        assert_eq!(deleted_label(&[&a, &c]), "2 bookmarks deleted");
    }
}
