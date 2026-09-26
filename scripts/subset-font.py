#!/usr/bin/env python3
"""Usage: uv run --with fonttools python scripts/subset-font.py /path/to/NotoSansSC[wght].ttf"""
import sys
from pathlib import Path
from fontTools.ttLib import TTFont
from fontTools.varLib.instancer import instantiateVariableFont
from fontTools import subset
font=TTFont(sys.argv[1])
instantiateVariableFont(font,{'wght':400},inplace=True)
chars=set(range(32,127))|set(range(0x3000,0x3040))|set(range(0xff00,0xffef))|{0x2026,0x00b7}
for hi in range(0xa1,0xf8):
    for lo in range(0xa1,0xff):
        try: chars.add(ord(bytes([hi,lo]).decode('gb2312')))
        except (UnicodeDecodeError,TypeError): pass
options=subset.Options();options.name_IDs=['*'];options.name_legacy=True
subsetter=subset.Subsetter(options=options);subsetter.populate(unicodes=chars);subsetter.subset(font)
for name in font['name'].names:
    if name.nameID in (1,4,6,16): name.string='RyzHubSans'.encode(name.getEncoding())
font.save(Path(__file__).resolve().parents[1]/'client/assets/fonts/RyzHubSans.ttf')
