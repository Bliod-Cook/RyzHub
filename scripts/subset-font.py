#!/usr/bin/env python3
"""Usage: uv run --with fonttools python scripts/subset-font.py /path/to/NotoSansSC[wght].ttf
       uv run --with fonttools python scripts/subset-font.py --recenter client/assets/fonts/RyzHubSans.ttf"""
import sys
from pathlib import Path
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont
from fontTools import subset

OUT = Path(__file__).resolve().parents[1]/'client/assets/fonts/RyzHubSans.ttf'

def recenter(font):
    """Noto Sans SC's line box (+1160/-288) centres 436 units above the baseline,
    but its glyphs centre near 370 (中 380, H 366), so text renders low. Keep the
    line height and move the box centre onto the glyphs."""
    total = font['hhea'].ascent - font['hhea'].descent
    centre = 370
    ascent = (total + 2*centre)//2
    descent = total - ascent
    font['hhea'].ascent, font['hhea'].descent = ascent, -descent
    font['OS/2'].usWinAscent, font['OS/2'].usWinDescent = ascent, descent

if sys.argv[1] == '--recenter':
    path = Path(sys.argv[2]); font = TTFont(path); recenter(font); font.save(path); sys.exit()
font=TTFont(sys.argv[1])
# Latin-1 Supplement and General Punctuation: GB2312 lacks •, the password mask.
chars=(set(range(32,127))|set(range(0xa0,0x100))|set(range(0x2000,0x2070))
       |set(range(0x3000,0x3040))|set(range(0xff00,0xffef)))
for hi in range(0xa1,0xf8):
    for lo in range(0xa1,0xff):
        try: chars.add(ord(bytes([hi,lo]).decode('gb2312')))
        except (UnicodeDecodeError,TypeError): pass
options=subset.Options();options.name_IDs=['*'];options.name_legacy=True
subsetter=subset.Subsetter(options=options);subsetter.populate(unicodes=chars);subsetter.subset(font)
# Instance after subsetting: the same outlines, without holding all glyphs' variations.
instantiateVariableFont(font,{'wght':400},inplace=True)
for name in font['name'].names:
    if name.nameID in (1,4,6,16): name.string='RyzHubSans'.encode(name.getEncoding())
recenter(font)
font.save(OUT)
