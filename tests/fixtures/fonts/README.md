# text test fonts

these unmodified noto fonts are test fixtures, not roblox font replacements.
`SourceSansPro.json` deliberately maps the enum-based test to noto sans so ci
can exercise `GetTextSize` without downloading roblox. normal sessions use
their selected content directory and its actual family descriptors.

from [notofonts/noto-fonts](https://github.com/notofonts/noto-fonts):

| file | sha256 |
| --- | --- |
| `hinted/ttf/NotoSans/NotoSans-Regular.ttf` | `b85c38ecea8a7cfb39c24e395a4007474fa5a4fc864f6ee33309eb4948d232d5` |
| `hinted/ttf/NotoNaskhArabic/NotoNaskhArabic-Regular.ttf` | `2f4b88e6ee50fa82c617e2d1d4ba18281cb1c6cd71c3af3ec64970c23995db4b` |

copyright 2018 the noto project authors. see `OFL.txt` for the complete sil
open font license 1.1. the font names and bytes are unchanged.
