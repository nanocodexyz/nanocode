# Attribution

Portions of this crate are derived from [clabby/tact](https://github.com/clabby/tact)
by Ben Clabby (revision `1d9ccaefd1d8613dab020812af04a91cd9b4c52c` and the earlier
revision `e20b1584642339546bb2310aad6968edeec66a53` imported through Nanocodex2),
modified for Nanocode. Those portions are distributed under the Apache License 2.0;
see `LICENSE-APACHE` at the repository root. Tact did not ship a separate NOTICE file.

## syntect

This crate's native syntax highlighting (`src/highlight/grammar.rs`, `parser.rs` and `highlighter.rs`) is a port of the parsing and highlighting code of [syntect](https://github.com/trishume/syntect) 5.3.0, and `src/highlight/syntaxes.rs` is generated from the default syntax set syntect 5.3.0 bundles (the Sublime Text grammars it ships in `assets/default_newlines.packdump`) together with the extra grammars listed under [Extra grammars](#extra-grammars).

syntect is distributed under the following license:

```text
MIT License

Copyright (c) 2017 Tristan Hume, Keith Hall, Google Inc and other contributors

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Extra grammars

`src/highlight/syntaxes.rs` also includes these Sublime Text grammars, taken from [bat](https://github.com/sharkdp/bat) v0.25.0 (revision `25f4f96ea3afb6fe44552f3b38ed8b1540ffa1b3`, `assets/syntaxes/02_Extra`). Where bat converts a TextMate grammar to `.sublime-syntax`, its converted file is used; bat is distributed under the MIT License and the Apache License 2.0, Copyright (c) 2018-2023 bat-developers.

| Grammar | Upstream | Revision | License |
| --- | --- | --- | --- |
| CMake | [zyxar/Sublime-CMakeLists](https://github.com/zyxar/Sublime-CMakeLists) | `eb40ede56c2d4d5a4a129b2a5bc7095a2df46bb1` | MIT, Copyright (c) 2012-2020 The Sublime CMake authors |
| Dockerfile | [asbjornenge/Docker.tmbundle](https://github.com/asbjornenge/Docker.tmbundle) | `0f6b7bc87acf684f7b0790fd480731ffb4615b87` | MIT, Copyright 2014 Asbjorn Enge |
| DotENV | [zaynali53/DotENV](https://github.com/zaynali53/DotENV) | `58201ba2abbbe6a12b2f6d0595efea3b388c1e38` | MIT, Copyright (c) 2016 Zayn Ali |
| Fish | [Phidica/sublime-fish](https://github.com/Phidica/sublime-fish) | `98316d4332936f74babb51cb56161410ae9d6e2c` | MIT, Copyright (c) 2017 Phidica Veia |
| Kotlin | [vkostyukov/kotlin-sublime-package](https://github.com/vkostyukov/kotlin-sublime-package) | `aeeed2780b04aea3d293c547c24cae27cafef0c5` | Apache 2.0, Copyright 2012-2014 Vladimir Kostyukov |
| Nix | [wmertens/sublime-nix](https://github.com/wmertens/sublime-nix) | `9032bd613746b9c135223fd6f26a5fa555f18946` | MIT, Copyright (c) 2016 Wout Mertens |
| Protocol Buffer | [VcamX/protobuf-syntax-highlighting](https://github.com/VcamX/protobuf-syntax-highlighting) | `1365331580b0e4bb86f74d0c599dccc87e7bdacb` | MIT, Copyright (c) 2013-2014 Yuxi Evan You |
| Swift | [quiqueg/Swift-Sublime-Package](https://github.com/quiqueg/Swift-Sublime-Package) | `fef17119ceef28a3c4b8cf72268d6192b595365d` | MIT, Copyright (c) 2014 Matthew Alexander |
| Terraform | [alexlouden/Terraform.tmLanguage](https://github.com/alexlouden/Terraform.tmLanguage) | `54d8350c3c5929c921ea7561c932aa15e7d96c48` | MIT, Copyright (c) 2016 Alex Louden |
| TOML | [jasonwilliams/sublime_toml_highlighting](https://github.com/jasonwilliams/sublime_toml_highlighting) | `f5a57e8bff694a4e6c52a491dae579aabc7427cf` | MIT, Copyright (c) 2021 Jason Williams |
| TypeScript, TypeScriptReact | [Microsoft/TypeScript-Sublime-Plugin](https://github.com/Microsoft/TypeScript-Sublime-Plugin) | `ba45efd058df5111837e30fb9598cfc8cbd51095` | Apache 2.0, Copyright Microsoft Corporation |
| Zig | [ziglang/sublime-zig-language](https://github.com/ziglang/sublime-zig-language) | `1a4a38445fec495817625bafbeb01e79c44abcba` | MIT, Copyright (c) 2018 Jeff Fowler |

The Apache 2.0 grammars are distributed under the Apache License 2.0; see `LICENSE-APACHE` at the repository root. The MIT grammars are distributed under the MIT License, whose text is reproduced in the syntect section above with each grammar's copyright line in place of syntect's.

## pulldown-cmark

The CommonMark and GFM parser in `crates/tui/src/commonmark` is ported from [pulldown-cmark](https://github.com/raphlinus/pulldown-cmark) 0.13.4 (its block pass, inline pass, scanners, link label matching and tree), cut down to the options Nanocode uses and modified for Nanocode. pulldown-cmark is distributed under the following license:

```text
The MIT License

Copyright 2015 Google Inc. All rights reserved.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in
all copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
THE SOFTWARE.
```

The parser's tables are generated from the [HTML Standard's named character references](https://html.spec.whatwg.org/entities.json) (WHATWG, CC BY 4.0) and the Unicode Character Database (`CaseFolding.txt` 18.0.0 and `UnicodeData.txt` 15.1.0), which is distributed under the following license:
## Unicode

The character property and case folding tables in `src/highlight/regex/unicode.rs` are generated from the Unicode Character Database 16.0.0 (`UnicodeData.txt`, `DerivedCoreProperties.txt`, `PropList.txt` and `CaseFolding.txt`), which is distributed under the following license:

```text
UNICODE LICENSE V3

COPYRIGHT AND PERMISSION NOTICE

Copyright © 1991-2026 Unicode, Inc.

NOTICE TO USER: Carefully read the following legal agreement. BY
DOWNLOADING, INSTALLING, COPYING OR OTHERWISE USING DATA FILES, AND/OR
SOFTWARE, YOU UNEQUIVOCALLY ACCEPT, AND AGREE TO BE BOUND BY, ALL OF THE
TERMS AND CONDITIONS OF THIS AGREEMENT. IF YOU DO NOT AGREE, DO NOT
DOWNLOAD, INSTALL, COPY, DISTRIBUTE OR USE THE DATA FILES OR SOFTWARE.

Permission is hereby granted, free of charge, to any person obtaining a
copy of data files and any associated documentation (the "Data Files") or
software and any associated documentation (the "Software") to deal in the
Data Files or Software without restriction, including without limitation
the rights to use, copy, modify, merge, publish, distribute, and/or sell
copies of the Data Files or Software, and to permit persons to whom the
Data Files or Software are furnished to do so, provided that either (a)
this copyright and permission notice appear with all copies of the Data
Files or Software, or (b) this copyright and permission notice appear in
associated Documentation.

THE DATA FILES AND SOFTWARE ARE PROVIDED "AS IS", WITHOUT WARRANTY OF ANY
KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT OF
THIRD PARTY RIGHTS.

IN NO EVENT SHALL THE COPYRIGHT HOLDER OR HOLDERS INCLUDED IN THIS NOTICE
BE LIABLE FOR ANY CLAIM, OR ANY SPECIAL INDIRECT OR CONSEQUENTIAL DAMAGES,
OR ANY DAMAGES WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS,
WHETHER IN AN ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION,
ARISING OUT OF OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THE DATA
FILES OR SOFTWARE.

Except as contained in this notice, the name of a copyright holder shall
not be used in advertising or otherwise to promote the sale, use or other
dealings in these Data Files or Software without prior written
authorization of the copyright holder.
```
