# Supermicro X9 (ATEN iKVM): protocol notes

How the ATEN iKVM on Supermicro X9-era BMCs works on the wire, as implemented in
`src/x9/`. Everything here was confirmed against a live BMC. The codec turned out to be
simple (raw little-endian RGB555, no compression, no crypto), so ipmicow speaks it in pure
Rust from the socket up. No vendor code is used or needed.

## Tested hardware

- Supermicro **B1SA4-F** (Atom S12xx SoC), BMC firmware **3.19**, IPMI 2.0
- BMC web server: ATEN embedded ("(c) ATEN International 2010")
- Reference viewer: **ATEN iKVM `V1.69.21`**, main class `tw.com.aten.ikvm.KVMMain`, needs Java 6

Other X9-generation boards running the same ATEN firmware family very likely behave the
same. Reports welcome.

## Two independent consoles

| | Text (SOL) | Graphical (iKVM) |
|---|---|---|
| Transport | IPMI 2.0 Serial-over-LAN, UDP 623 | ATEN proprietary, **TCP 5900** |
| Standard? | Yes (open) | No: despite port 5900 it is **not** plain RFB/VNC |
| Client | `ipmitool -I lanplus -H <bmc> -U ADMIN -P ADMIN sol activate` | ipmicow |

SOL covers text, boot and console-redirection work. ipmicow targets the graphical channel.

## Minting a console token

The token the viewer presents is ephemeral and tied to a web session, so the client logs
in and mints one itself, exactly as the web UI's "Launch Console" button does
(`src/x9/mint.rs`).

1. **Login**: `POST http://<bmc>/cgi/login.cgi` with `name=<user>&pwd=<password>`.
   The response sets cookie `SID=<16 chars>`. It sends two `Set-Cookie: SID=` headers,
   the first clearing the cookie and the second setting it; keep the last non-empty one.
   - Use **plain HTTP (port 80)**. The BMC's 2013-era HTTPS cipher suites are refused by
     modern TLS stacks (rustls *and* macOS Secure Transport) with a "Cipher Suite
     negotiation failure". Over HTTP the `SID` cookie isn't `Secure`-flagged, so it flows
     fine, and the video channel is raw TCP regardless.
2. **Mint**: `GET /cgi/url_redirect.cgi?url_name=ikvm&url_type=jwsk` returns a `.jnlp`
   (the SOL equivalent is `url_name=sol&url_type=jwss`).
   - **Gotcha:** the request must carry the browser's client-side `language` cookie (set
     by the login page's JavaScript) and a `Referer`. A bare `SID`-only request returns
     **500** (a server-side template crash). ipmicow sends
     `Cookie: langSetFlag=0; language=English; SID=<sid>` and
     `Referer: <base>/cgi/url_redirect.cgi?url_name=man_ikvm`.
   - The web UI's button just runs `deployJava.launch(url)`, i.e.
     `document.location = url`. The JNLP is served as `application/x-java-jnlp-file`, so
     the browser downloads it. (Safari doesn't log download navigations in its network
     panel, which makes the request look like it never happened.)
3. **Log out** (`GET /cgi/logout.cgi`): the BMC allows only ~2 concurrent web sessions,
   so don't squat on one.

**JNLP arguments** (`application-desc`):

| idx | example | meaning |
|----|-------|---------|
| 0 | `<bmc>` | BMC host |
| 1 | `abcdefghijklmnop` | ephemeral session token (used as "user") |
| 2 | `abcdefghijklmnop` | the same token (used as "password") |
| 3 | `null` | — |
| 4 | `5900` | **video/KVM TCP port** |
| 5 | `623` | IPMI UDP port |
| 6 | `2` | (guess) privilege / encryption flag |
| 7 | `0` | (guess) SOL-vs-KVM flag |

The token is minted per click and stale JNLPs don't work.

## Handshake

RFB `003.008` ProtocolVersion exchange, then security type **16**, then **ATEN auth**:
read the 24-byte server-info block, send the session token **twice** (each NUL-padded to
24 bytes), and read a 4-byte result where `0` means OK. Then ClientInit(0) and ServerInit.
There is **no crypto in the login path**: the web-minted token is the credential, which is
why the JNLP carries it as both "user" and "password".

```
<- "RFB 003.008\n"          (12)        server ProtocolVersion
-> "RFB 003.008\n"          (12)        client ProtocolVersion
<- nTypes:u8, types:u8[n]                offered security types (= [16])
-> 16:u8                                 select ATEN auth
<- serverInfo               (24)         magic 0xBE1FF9AF + params (NOT a challenge)
-> token padded to 24       (24)         "user"
-> token padded to 24       (24)         "password"
<- result:u32                            0 == authenticated
-> 0:u8                                  ClientInit (shared)
<- width:u16,height:u16,pf[16],nameLen:u32,name   ServerInit
<- 12-byte ATEN trailer                  skip 4, privId:u32, 4 bytes
```

**The ServerInit trailer matters.** ATEN's `ProcServerInit` reads the standard ServerInit
**plus a 12-byte trailer**. Miss it and everything after is mis-framed (an early symptom
was a bogus "0 rectangles"). With it consumed, a standard `FramebufferUpdateRequest`
makes the server stream video.

## Message stream

Per server message (`RFBScreen::ScreenDecode`):

```
msgType:u8
  0x00  FramebufferUpdate: pad:u8, nRects:u16, then per rect:
        x:u16 y:u16 w:u16 h:u16  enc:u32  mode:u32  len:u32  data[len]
  0x04  cursor position: 24 bytes (5x u32 + flag u32); ipmicow ignores it
  0x16  keepalive: one byte
  0x39  connected-users list: u32 count, then count x 260-byte "<sid> <user> <ip>\0" records
```

## Video: raw RGB555

The video on these boards is **not compressed**. The rect `enc` (87..97) selects one of
four decoders in the vendor library (`AST`, `Hermon`, `Pilot3`, `Yarkon`); these boards
are **Hermon** (Nuvoton WPCM450). Its payload starts with a 10-byte header:

```
mode:u8 (1 = full frame, 0 = changed tiles)  pixfmt:u8 (0 = 16bpp RGB555)
count:u32 BE  len:u32 BE
```

- **Full frame** (`mode = 1`): raw **little-endian RGB555** pixels follow, `w*h*2` bytes.
  (Its `0x12345678` "magic" is just the count field read as a u32.) Confirmed
  byte-for-byte against the BMC's own snapshot CGI
  (`/cgi/url_redirect.cgi?url_name=Snapshot&url_type=img`, a server-rendered BMP) at both
  752x413 and 1024x768.
- **Incremental** (`mode = 0`): `count` changed 16x16 tiles, each
  `[4 bytes ?, row:u8, col:u8] + 16*16*2 bytes RGB555`, placed at `(col*16, row*16)`.
  Applying incremental updates on top of a full frame was verified pixel-exact against a
  fresh full frame (0 of 786,432 pixels differ).

ipmicow requests incremental updates in a loop, so only changed tiles cross the wire.
That keeps bandwidth low and the input loop snappy. The Yarkon decoder adds an
`0xFE`/`0xFF` RLE/Huffman wrapper, and the AST decoder is a JPEG-family codec; neither is
used by these boards.

**Resolution** changes at runtime (BIOS → bootloader → OS). A full-screen rect with new
dimensions reallocates the framebuffer and recreates the window. A rebooting BMC with no
stable signal emits rects with garbage dimensions (e.g. 64896x65056); ipmicow drains and
skips them rather than dropping the session.

## Keyboard

`RFBKeyboard::Sendkey` sends plaintext **USB HID usage codes** in an 18-byte message:

```
04 00 <down:u8> 00 00 <usage:u32 BE> 00*9
```

X11 keysyms do **not** work: the BMC emulates a USB keyboard, and its LED logic keys off
HID 0x39/0x47/0x53.

## Inside the vendor viewer

Useful if you're bringing up a related ATEN board. The Java jar is a thin shell; protocol,
crypto and codecs live in per-arch native libraries loaded over JNI:
`libiKVM64.{jnilib,so}` (connection, auth, video) and `libSharedLibrary64` (virtual
media). The **macOS `.jnilib` is not stripped**, so its C++ symbols make it a labelled
spec (the Linux `.so` is stripped). Both are x86_64 only.

```
TcpSocket (plain TCP :5900)
  -> NtwStream::Connect / StreamRecv
     -> RFBProtocol::InitHandShake + Authenticate(Userinfo_t)
        RMConnection::{CheckVaildUser, ConnKeepActive, QosControl}
  crypto (unused on this link):
     video  -> RC4    ast2100::{DecodeRC4_setup, RC4_crypt}
     input  -> AES128 RFBKMCryto::{aes_set_key, RFB_AES128_EventCryto}
  decode:
     RMDecoder::GetDecoder(Decodeinfo) -> ASTVideoDecoder | HermonVideoDecoder
                                        | Pilot3VideoDecoder | YarkonVideoDecoder
  input:
     RFBKeyboard::Sendkey(int,int)   RFBMouse::SendMouse(MouseInfo_t)
```

Headers on this link arrive in cleartext, which matches `ipmitool sol info` reporting
Force Encryption = false. A BMC with encryption forced would need the RC4/AES paths.

## Not implemented

- Mouse (`RFBMouse::SendMouse`), the server cursor (type 4), and virtual media
  (`libSharedLibrary64`).
