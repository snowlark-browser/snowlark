// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// The coarse pixel check, evaluated in a blank Chrome page: decodes the two screenshots (data URLs
// __CHROME__ and __SNOWLARK__), crops both to __WIDTH__ x __HEIGHT__ CSS px, and splits them into
// __BLOCK__ px blocks. A block is content when some pixel in it differs from the image's background (its
// most common colour) by more than __THRESHOLD__ (sum of the channel differences). Returns the map of the
// blocks, one character per block and a row per line: '#' content only in Chrome, '+' content in both,
// '-' only in Snowlark, '.' background in both.
(async function () {
  var W = __WIDTH__, H = __HEIGHT__, B = __BLOCK__, T = __THRESHOLD__;
  function load(src) {
    return new Promise(function (resolve, reject) {
      var img = new Image();
      img.onload = function () { resolve(img); };
      img.onerror = function () { reject(new Error('image did not decode')); };
      img.src = src;
    });
  }
  function pixels(img) {
    var c = document.createElement('canvas');
    c.width = W; c.height = H;
    var g = c.getContext('2d', { willReadFrequently: true });
    g.fillStyle = '#fff';
    g.fillRect(0, 0, W, H);
    g.drawImage(img, 0, 0);
    return g.getImageData(0, 0, W, H).data;
  }
  function background(d) {
    var counts = new Map(), best = 0, key = 0;
    for (var i = 0; i < d.length; i += 4 * 7) {
      var k = ((d[i] >> 3) << 10) | ((d[i + 1] >> 3) << 5) | (d[i + 2] >> 3);
      var n = (counts.get(k) || 0) + 1;
      counts.set(k, n);
      if (n > best) { best = n; key = k; }
    }
    return [((key >> 10) & 31) * 8 + 4, ((key >> 5) & 31) * 8 + 4, (key & 31) * 8 + 4];
  }
  function blocks(d) {
    var bg = background(d), cols = Math.ceil(W / B), rows = Math.ceil(H / B);
    var out = new Uint8Array(cols * rows);
    for (var y = 0; y < H; y++) {
      var row = (y / B) | 0;
      for (var x = 0; x < W; x++) {
        var i = (y * W + x) * 4;
        var diff = Math.abs(d[i] - bg[0]) + Math.abs(d[i + 1] - bg[1]) + Math.abs(d[i + 2] - bg[2]);
        if (diff > T) out[row * cols + ((x / B) | 0)] = 1;
      }
    }
    return { cols: cols, rows: rows, map: out, bg: bg };
  }
  var a = blocks(pixels(await load('__CHROME__')));
  var b = blocks(pixels(await load('__SNOWLARK__')));
  var lines = [];
  for (var r = 0; r < a.rows; r++) {
    var line = '';
    for (var c = 0; c < a.cols; c++) {
      var i = r * a.cols + c, x = a.map[i], y = b.map[i];
      line += x && !y ? '#' : x && y ? '+' : y ? '-' : '.';
    }
    lines.push(line);
  }
  return JSON.stringify({ block: B, bg: [a.bg, b.bg], map: lines });
})()
