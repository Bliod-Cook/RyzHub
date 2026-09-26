# Bundled fonts

`RyzHubSans.ttf` is a weight-400, renamed subset of Noto Sans SC under the accompanying OFL license. Source: https://github.com/google/fonts/tree/main/ofl/notosanssc (retrieved 2026-09-25). It covers GB2312, ASCII, CJK punctuation and full-width punctuation (7,726 requested codepoints), keeping the initial Chinese font to about 2.3 MiB. Font family is overridden as `RyzHub Sans` in pubspec.

`web/font-fallback/notocoloremoji/v32` hosts the Google Fonts Noto Color Emoji shards used by the mock agent's crab character and SDK response symbols; its license is `web/font-fallback/NOTO-EMOJI-OFL.txt`. Flutter's fallback base points to this local directory so the client does not make runtime font requests to gstatic. Additional writing systems or uncommon glyphs require additional licensed shards; this PoC is not a complete Unicode font distribution.

The M3 symbol shard `Yq6P-KqIXTD0t4D9z1ESnKM3-HpFabsE4tq3luCC7p-aXxcn.9.woff2` was retrieved from `https://fonts.gstatic.com/s/notocoloremoji/v32/` on 2026-09-26. SHA256: `2c6df98bda8729b0df325c2218482d6d4772f44e78694663778e023d4d9acc7a`. It uses the same bundled OFL license.
