# IBM IMM2 (Avocent DVC): protocol notes

How the KVM on IBM's Integrated Management Module 2 works on the wire, as implemented in
`src/imm2/`. Unlike the Supermicro X9, the IMM2 uses Avocent's **APCP/BEEF** framing and
**DVC** (Dambrackas Video Compression) over **TCP 3900**. Everything below was
reverse-engineered from the reference Java viewer and confirmed against a live BMC unless
marked otherwise.

## Tested hardware

- **IBM System x3750 M4**, IMM2 firmware **8.00**.
- The IMM runs on standby power, so it's reachable long before the host's (slow) POST.
- Remote Control was **not** licence-locked on the test box. On some IMM2 systems the
  graphical console needs a Features on Demand key; ipmicow can't work around that.

The same Avocent APCP/DVC stack is used by other IMM2 systems (x3550/x3650 M4 and
friends), Dell iDRAC6/7 and Avocent DSR/MergePoint switches, so the decoder should carry
over. The firmware-specific parts are the web-UI token mint and the login details.

## Two consoles

| | Text (SOL) | Graphical (KVM) |
|---|---|---|
| Transport | IPMI 2.0 SoL, UDP 623 | Avocent proprietary, **TCP 3900** |
| Standard? | Yes | No: raw binary (the BMC resets on TLS/HTTP probes) |
| Codec | n/a | **DVC** |
| Encryption | — | **Off by default** (TLS if "Encrypt KVM data" is ticked) |

Plain IPMI already covers power, sensors, SEL and SOL:

    ipmitool -I lanplus -H <bmc> -U USERID -P PASSW0RD chassis status
    ipmitool -I lanplus -H <bmc> -U USERID -P PASSW0RD sol activate

(`PASSW0RD` has a zero, not the letter O.) `:3900` carries both keyboard/mouse (`kmport`)
and video (`vport`).

## Minting a console token

The web UI's "Remote Control" button offers a Java or ActiveX client. It builds a launch
string and opens `viewer(<launchString>).jnlp`, which the BMC generates dynamically. The
JNLP's `user=0x…` argument is a per-session 32-bit token: the only credential the KVM
port needs. ipmicow replicates this (`src/imm2/mint.rs`):

1. `POST https://<bmc>/data/login` with form `user=<user>&password=<password>`, which
   sets an `_appwebSessionId_` cookie.
2. `GET https://<bmc>/designs/imm/viewer(<ls>).jnlp` with that cookie, where `<ls>` is the
   launch string built by `serverRemoteControl.js`:

       ip @ webPort(443) @ ipFormat @ timestampMs @ encrypt(0/1) @ exclusive(1/0)
          @ platform(Win=0/other=1) @ (jnlp|activex) @ user @ knock @ preemptTimeout
          @ reserveId @ authKey @ RP_javaSecSupport

   ipmicow sends `<bmc>@443@0@<ms>@0@1@1@jnlp@<user>@0@0@0@0@1`. The third field is the
   IP *format* (from `getIPAddressAndFormat`), not an IP.
3. Pull `user=0x…` out of the JNLP. A sample argument list:

       ip=<bmc>   kmport=3900   vport=3900
       user=0x12345678    passwd=            (empty; the token is the credential)
       immversion=2  vm=1  color=2  power=1  webuser=USERID

**Use the full launch string.** A short form (`<bmc>@443@<bmc>@<ms>@0@1@1@jnlp@<user>@0@0`)
still mints a working KVM token, but the endpoint reports `result: fail` and the JNLP
says **`vm=0`**. That session has no virtual-media binding, and VM logins are refused with
1 ("invalid user name") or 3 ("channel access denied"). The full form gives `vm=1`.

## The reference client

Served from `https://<bmc>/designs/imm/aessrp/`:

- **`avctIBMViewer__V092815.jar`**: the main jar, **pure Java**. The protocol and the DVC
  decoder live here.
- `avctKVMIO{Mac64,Win32,Win64,Linux32,Linux64}`: native libraries for local
  keyboard grab, LED status and cursor only. They have nothing to do with the wire protocol.
- `avctVMAPI_DLL*`: native virtual-media library (`libVMAPI_DLL`). It handles USB reset,
  covered below.

The jars target Java 1.5/1.6 and run on JDK 8. To read them:

    curl -sk -o avctIBMViewer.jar https://<bmc>/designs/imm/aessrp/avctIBMViewer__V092815.jar
    jadx --show-bad-code -d viewer-src avctIBMViewer.jar

(jadx reports several hundred non-fatal errors; the output is still usable.)

**Obfuscation:** Zelix-style string encryption, a `z[]` `String[]` plus a `z(char[])` XOR
decoder (the key cycles through position % 5). In-code strings such as opcode and argument
names are decrypted at runtime, so they don't grep. Recover them by calling `z()` through
reflection, or by reimplementing it. Byte-level logic (DataInput/OutputStream reads and
writes, offsets, opcode switches) is fully readable. Resource bundles
(`com/avocent/client/res/Res_*.properties`) are plaintext.

**Code map** (obfuscated package → role):

- `com.avocent.ibmc.kvm.Main` → `IBMViewerMainController.initialize(cfg, args)` → `startApp()`
- Transport / APCP: **`com.avocent.d.a.a`** (connect + handshake); session
  `com.avocent.d.c.b` (AVSPKVMSession); readers `d.c.q` (primary) and `d.c.r` (video)
- BEEF messages: `com.avocent.c.c.*` (base `c.c.a` = `"BEEF"` + type + len; login `c.c.bb`)
- Message dispatch: `com.avocent.d.f.a.a` (type → class) and `com.avocent.d.f.a.b` (handlers)
- **DVC decoder: `com.avocent.kvm.a.a.*`** (`c` = opcode loop, `d` = DVC15, `g` = DVC7,
  `f` = DVC7Gray, `e` = DVC23); framebuffer `com.avocent.b.d.b.b`
- ⚠️ `a.a.a.*` is **not** the decoder. It's a vendored Monte-Media QuickTime *writer*
  used for session recording.
- Virtual media: `com.avocent.vm.*` (+ `vm.jni.RPVMNativeLibrary`)

## Wire protocol

Everything is **big-endian** (Java `DataOutputStream`). Each socket starts with an APCP
handshake. After that, length-prefixed BEEF messages and `VID` video frames are
interleaved on the same stream.

### 1. APCP handshake (`com.avocent.d.a.a`)

The basic request is 53 bytes (`ipmicow debug imm2-probe <bmc>` sends it and dumps the
reply):

    "APCP"            41 50 43 50
    int32  len        00 00 00 35   (= 53, whole frame)
    u16    type       01 00         (256)
    u16    encrypt    00 00         (0 = plaintext; 258 when "Encrypt KVM data" is on)
    u8     cmd        03            (3 = primary, 4 = secondary/video)
    u8     ver_major  02
    u8     ver_minor  28            (= 40, so version 2.40)
    u8     0          00
    int32  caps       00 00 00 05   (client capability flags)
    u8     nonce_len  20            (32)
    byte[32] nonce                  (random; used for reconnect keying, not auth)

**BMC reply (53 bytes):** `APCP`, len 53, type **0x8100**, u16 APCP version, u8.u8 peer
version **2.40**, int32 caps **1**, u16 redirect port **0**, u8 payload length 0, zero
padding. `caps & 1` means a plaintext **type-1** connection; `& 4` would mean TLS and
`& 512` reconnect support (both off here). A non-zero redirect port would mean "reconnect
on that port".

The live viewer sends the reference client's **68-byte** variant instead: the same
header (with `0x0102` in the encrypt/version word), caps **1541**, then a trailer of
`u8 0, u16 0, u16 heartbeat 180, u16 timeout 30, u32 0, u32 0`.

### 2. BEEF framing (`com.avocent.c.c.*`)

    "BEEF"           42 45 45 46
    u16  type
    u16  total_len                  (whole message, including this 8-byte header)
    payload[total_len - 8]

Server macros arrive in identical framing with the magic **`CSCO`**; the receiver
doesn't check the magic.

### 3. Login (`com.avocent.c.c.bb`)

Sent on the primary socket right after the handshake. With APCP version ≥ 2 the type is
**258** and the message is **217 bytes**:

    u8        user_len
    char[96]  username      the JNLP `user=0x…` token, zero-padded
    u8        pass_len      0
    char[96]  password      empty
    byte[8]   n             0
    u8        o             kmport & 0xFF (0x3C), as the reference sends it
    u8        p             0
    int32     r             client random (1..10^7, fresh per login)
    u8        q             share mode: 0 normal, 1 shared

The reply is **0x8305** LoginResponse (0x8300 for the older type-256 login): [0] status,
[4..7] **appliance random**, [8..9] flags. Status 0 = OK, 1/2 bad login, 3 access denied,
4/12 **KVM in use**, 8 all channels in use, 15/16 sharing denied or timed out. Flags
bit0 means video stays on the primary socket (so no cmd-4 connection is needed), bit5
asks for a refresh, bit6 means view-only; the test box sends `0x0011`. (A reading of the
reference handler suggested the flags sit at [3], with a name length and name from [8];
the live bytes fit [8..9] better. Treat that layout as unconfirmed.)

### 4. Start video

Right after login the client sends a control burst. The first message is a 12-byte
`APCP` cmd 0x04 (`41 50 43 50 00 00 00 0c 04 00 00 00`). Then come BEEF **0x030E** (video
enable, `01 01`), **0x0302** (display area 1024x768) and **0x0304**. **One connection is
enough:** video then flows on the primary socket.

**The secondary (cmd 4) connection is optional.** The reference client opens it only when
the login reply's "video on primary" flag (bit0) is clear. When it does:

1. It sends a 53-byte APCP request with cmd 4.
2. It sends a non-BEEF "Video Channel Auth" (`c.c.yb`): `00 00 00 00 01 01 00 10` +
   int32 client random + int32 appliance random.
3. The server answers with `vb` (type 0x84), and start-video goes out on the primary.

### 5. Video: `VID` frames + DVC

Video uses its own framing, not BEEF:

    "VID\0"          56 49 44 00
    u16  type                       colour mode (see below)
    u16  total_len                  whole fragment, including this 8-byte header
    "deadbeef"       de ad be ef
    u16  height                     e.g. 769
    u16  width                      e.g. 1024
    u8   flags                      bit0 = first fragment, bit1 = last, bit2 = incremental
    3 bytes                         unknown
    …    DVC opcode data

One screen spans several fragments, and their DVC data **concatenates**. An opcode can
straddle a fragment boundary, so reassemble the whole frame before decoding.

**DVC decoder** (`src/imm2/dvc.rs`, ported from `com.avocent.kvm.a.a`). Pixels land in a
persistent row-major ARGB framebuffer (stride = width, no y-flip), so inter-frame ops
work. The top 3 bits of each lead byte pick the op:

| `lead & 0xE0` | op | action |
|---|---|---|
| 0x00 | No-Change | skip N px (keep the previous frame's pixels) |
| 0x20 | Copy-Left | repeat the pixel at cursor-1, N times |
| 0x40 | Copy-Above | copy from the row above (cursor - width), N times |
| 0x60 | Make-Series | two colours from history plus a selector bitmap: the low nibble covers 4 px; with bit4 set, 7-px continuation bytes follow while their bit7 is set |
| ≥ 0x80 | Make-Pixel | one literal pixel in the active colour mode |

Run length N: the low 5 bits of the lead, extended by following bytes whose top 3 bits
match the same op, as LSB-first 5-bit chunks (≤ 25 bits). DVC7 and DVC7Gray add 2.

| VID type | Mode | Make-Pixel |
|---|---|---|
| 0x8601 | DVC15 | RGB555: `v = (lead & 0x7F) << 8 \| next`, 5-bit fields `<< 3` |
| 0x8602 | DVC7 | 128-colour palette (a 5x5x5 cube on {0,70,127,191,255} + 3 greys, permuted) |
| 0x8603 | DVC7Gray | 7-bit grey `<< 1` |
| 0x860A | DVC23 | 23 bits over 3 bytes: R = v>>15, G = (v>>7)&0xFF, B = (v&0x7F)<<1 |

The BMC negotiates DVC15 unless a depth is chosen. DVC15 decoding is verified
pixel-perfect against the reference viewer. DVC7, DVC7Gray and DVC23 are implemented but
less exercised.

**Overscan:** frames are 1024x**769**, but the visible display is 1024x**768**. The extra
bottom scanline is garbage (hundreds of distinct colours where a background row has one).
ipmicow keeps all 769 rows for inter-frame decoding and displays `min(h, 768)`.

### 6. Input and keepalive (client → server, 16-byte BEEF)

All are sent on the primary socket and unit-tested byte-exact against the reference
client.

- **Keyboard**, type `0x0200` (`c.c.e`): `[0x00, up_flag, hid:u16, 0,0,0,0]`. `up_flag`
  is **inverted** (0 = make, 1 = break), and `hid` is a USB HID usage. Modifiers are their
  own key events (no bitmask), so `A` is LShift down, a down, a up, LShift up.
- **Mouse**, type `0x0201` (`c.c.fc`): `[0x00, buttons:u8, x:u16, y:u16, wheel:u16]`.
  Buttons bit0/1/2 = L/R/M. x/y are **absolute pixels** in the announced 1024x768 (not
  0..0x7FFF), and wheel is a signed delta. (`0x0209` is an unused relative variant.)
- **Keepalive**, type `0x0400` (`c.c.r`): `42 45 45 46 04 00 00 10` + 8 zero bytes, every
  **10 s**. No reply to any server ping is required.

In the reference client, keyboard, mouse and keepalive all write to the cmd-3 primary
socket. The mouse writer discards everything until start-video. The secondary socket
carries no input.

### 7. Control messages ipmicow answers

| Sent | When | Bytes |
|---|---|---|
| Video Ack (`c.c.db`, type 0x0000) | after **every** `VID` message | payload `01 00 …` |
| ScreenRefresh (`c.c.p`, 0x0301) | after each 0x8201 DisplayResolution | zero payload |
| SetDVCColorDepth (`c.c.m`, 0x0403) | on the first `VID` message and whenever the type changes | `[auto?1:0, bits, colour?1:0, user-set?1:0, 0…]` |
| Server macro request | after a status-0 login | 68-byte APCP with cmd **0x9F**, version 2.37, caps 4 |
| Keyboard LED Request (`c.c.f`, 0x0204) | Keys ▸ Check Host Keyboard LEDs | zero payload |

The reference client's Colour Depth menu offers 7/9/12/15/18/21/23-bit colour and 4–7-bit
grey (`com.avocent.e.g`). Depths above 15 switch the stream to DVC23. The BMC confirms with
0x8204 DVCColorModeResponse ([0] mode).

### 8. Server → client messages

Offsets are into the payload after the 8-byte header.

| Type | Name | Meaning / what the client does |
|---|---|---|
| 0x8305 / 0x8300 | LoginResponse | see Login above |
| 0x8302 | ProtocolVersion | [0].[1] = 2.40; ignored |
| 0x8430 / 0x8431 | AvailableServerNames / SelectedServerUpdate | power-control target list (empty here); ignored |
| 0x8102 | MouseAccel | [0] = 1: **absolute** mouse (0 none, 2 relative) |
| 0x8200 / 0x8206 | InputResolution | hres, vres, platform; nothing to send |
| 0x8201 | DisplayResolution | hres, vres; **answer with ScreenRefresh 0x0301** |
| 0x8204 | DVCColorModeResponse | [0] colour mode (15 by default) |
| 0x8100 | KeyboardLED | [0] host LED bits: Num 1, Caps 2, Scroll 4, Compose 8, Kana 16 |
| 0x8101 | MouseAck | [0] count of mouse messages processed |
| 0x8301 | UserDisconnectPending | [0] reason: 0 admin, 1 timeout, 2 reboot, 3 upgrade, 4 preempt, 5 unshare, 6 exclusive |
| 0x8303 | UserDisconnectPendingTimeout (preempt) | see Sharing below |
| 0x8401 | SharingRequest | see Sharing below |
| 0x9F00 | CustomServerMacro (CSCO) | the BMC's macro text |
| APCP 0x8400 | server heartbeat | ignored |

**KeyboardLED doubles as an end-to-end check.** The host OS drives those LEDs, so a
change after toggling Caps or Num Lock proves the host processed the key.

Other names recovered from the string-decrypted jar (each class's `p()` returns its name):
0x8202 VideoSetupData, 0x8203 Scaling, 0x8410 SharedUserResponse, 0x8412 Locked, 0x8413
Reserved, 0x8420 UserPrivilegeParameterResponse, 0x8432 ServerStatusUpdate, 0x8434
RequestCredentials, 0x8435 Chat, 0x9E10/0x9E11 MgmtBootDevice struct/result, 0xFFFF NoOp.
Client → server: 0x0202 MouseOrigin, 0x0208 FocusControl, 0x0310 SetVideoParameters,
0x0424 SetPowerState, 0x0427 RequestCredentials, 0x0428 Chat.

### 9. Sharing and preemption

"Exclusive" is the `exclusive` field of the launch string used to mint the token
(ipmicow sends 1). If a session already exists, a normal login gets status 4/12. The
reference client then asks "share?" and logs in again with `q = 1`. The owner gets
**0x8401 SharingRequest** (`[id:u16][type][user…]`) and answers SharingResponse
**0x0413** `[answer, 0, id_hi, id_lo, 0…]`, where 1 = reject, 2 = allow, 3 = read-only and
0 = timeout. ipmicow allows with full access, since the other party is almost certainly
you.

Preemption is driven by the launch string's `knock` / `preemptTimeout` (both 0 in ours)
and by BMC logic. The owner gets **0x8303**
(`[reason][status][timeout:u32][user len][user 96][addr len][addr 40][action timeout]`)
and answers PreemptionResponse **0x0416** `[accept, 30, 0…]`. Accepting also shuts the
reference viewer down, and ipmicow does the same rather than reconnecting and
ping-ponging.

**There is no client-side "kill old session" message.** The 68-byte APCP reconnect id
only resumes this client's own session, and only when `caps & 512`, which this BMC
doesn't set. Neither the KVM nor the VM client tells the BMC to drop a previous session
at connect time.

### 10. Closing

"AVSP Close Connection" (`c.e.a`) is `41 50 43 50 00 00 00 10 01 FF 00 00 00 00 00 00`.
The reference client sends it only when the BMC's setup reply has `caps & 512`; this BMC
reports caps 1, so it just closes the socket. ipmicow does the same, with
`shutdown(Both)` on quit. The BMC then unplugs its virtual USB keyboard on the host (a
`USB disconnect` in dmesg), and the next session plugs a new one in.

## Virtual media (AVMP): just enough for USB Reset

The reference client's Virtual Media window has a **USB Reset** button ("can affect all
input to the target device including virtual media, keyboard and mouse"). The BMC
unplugs and re-plugs its virtual USB devices, which unsticks a host that has lost its
keyboard. It's a native call, `RPVMNativeLibrary.RequestUsbReset()` in
`libVMAPI_DLL.jnilib` (x86_64 Mach-O with C++ symbols intact), sent over a
virtual-media connection. ipmicow implements it in `src/imm2/vmedia.rs`, behind
Keys ▸ Reset Host USB.

Every packet starts with a 12-byte header: 4-byte magic, u32 total length, u16 type and a
type-specific u16 (`AVMPPacket::encodeHeader`). Session-level packets use magic `APCP`;
the rest use `AVMP`.

1. **Session request**: a second TCP connection to **3900**, alongside the live KVM
   session. It uses the same 68-byte layout as the KVM connect:
   `APCP 00000044 0100 0102 | [12] protocol type 2 | [13..14] AVMP version 1.13 | [15] 0 |
   [16..19] caps 0x605 | [20] 32 | [21..52] random | [53..55] 0 | [56..57] reconnect timeout 180 |
   [58..59] keepalive 0 | [60..63] session id 0 | [64..67] 0`.
2. **Setup reply**: APCP type **0x8100**, carrying the APCP version, AVMP version and
   caps/port. `caps & 4` means TLS on the given port, which ipmicow reports as an error.
   The test box replied `APCP 00000044 8100 0102 | 01 02 | 0000 0601 …`: AVMP 1.2, caps
   0x601 (reconnect-capable, plaintext).
3. **Login**. The reference client uses **RandomLoginMessage** (AVMP **0x0102**, 22 bytes):
   header | u32 client random | u32 appliance random | u8 0 | u8 preempt. It echoes the KVM
   session's randoms. The fallback is **LoginMessage** (AVMP **0x0100**, 153 bytes):
   header | [12] user length | [13..108] user (≤ 95) | [109..140] password, plain,
   zero-padded (≤ 32) | [141..148] rip id 0 | [149] channel 0 | [150] port 0 | [151] 0 |
   [152] preempt.
   On the test box the KVM login reply always carries appliance random **0**, and the VM
   server answers the random login with status 1. So ipmicow sends **one** `LoginMessage`
   with the IMM account (`--user` / `--password`), and only one, because the IMM locks
   accounts after repeated failures.
4. **Login reply**: AVMP **0x8100**: [12] status (0 = OK), [13] flags, [14] name length,
   [15..] name. On success the library queries the virtual disks and starts a 10-second
   heartbeat (AVMP 0x0400, 12 bytes). Nothing attaches virtual drives to the host
   automatically.
5. **UsbReset**: AVMP **0x0420**, 16 bytes, with word10 = remote disk id (0):
   `41564D50 00000010 0420 0000 00000000`. **The sequence that works on the host:** login →
   GetVDiskInfo (`0200 FFFF`) and its 0x8200 reply → UsbReset → APCP heartbeat → **stay
   connected ~5 s** → SessionClose. The host logs a `-71`, a clean USB disconnect, then
   re-enumerates keyboard and mice; the KVM session carries on throughout. The BMC acks
   with AVMP **0x8410** DeviceStatus. An attempt that skipped the query and closed
   straight after the reset got the same ack, but nothing happened on the host.
6. **Close**: SessionClose, APCP type **0x01FF**, 16 bytes:
   `41504350 00000010 01FF 0000 00000000`.

> **Security note.** This firmware sends no nonces. The appliance random is 0 in every
> login, and both APCP setup replies carry a zero-length server random. The
> random-number VM login would succeed with `client random + 0`, so the virtual-media
> channel is authenticated only by a client-chosen value that travels in clear in the
> KVM login. Keep the IMM on a trusted management network.

## Spotted, not implemented

- **Power control**: SetPowerState 0x0424, the power target list 0x8430/0x8431,
  ServerStatusUpdate 0x8432. `ipmitool` already covers power, and a power menu next to
  the keys is a footgun.
- **Next boot device**: MgmtBootDeviceSelect 0x9E90 → 0x9E10 / 0x9E11.
  `CAPABILITY_NEXT_BOOT` isn't enabled for IBM, so it's probably unsupported.
- **Chat** (0x0428 / 0x8435) and RequestCredentials (0x0427 / 0x8434).
- **DVR / recording, video export, scaling, mini mode**: client-side features of the
  Java viewer.
- **Encryption**: `caps & 4` in an APCP setup reply means TLS on the port it names. Not
  implemented on either channel; both report it as a clear error.
- **Virtual media drive mapping** (mount an ISO as a USB CD). Scoped from `libVMAPI_DLL`
  (word10 = remote disk id unless noted):
  - `0x0200` GetVDiskInfo, sent as `AVMP 0000000C 0200 FFFF`. The `0x8200` VDiskInfo reply
    has [12..13] slot count, then one 4-byte block per slot (u8, u8, u16). The test box
    reports 16 slots, each `00 00 0001`, with nothing mapped.
  - `0x0211` VDiskRequest2 (map), 26 bytes: [12..15] block size, [16..19] capacity in
    blocks, [20] flags (1 = read-only), [21] min(capacity, 255), [22..23] cylinders,
    [24] heads, [25] sectors.
  - `0x8300` ReadRequest from the BMC: [12..15] start block, [16..19] block count,
    [20..23] max blocks per packet. Answer with `0x0300` ReadResponse: [12..15] start
    block, [16..19] block count, then the data.
  - `0x8310` VDiskWrite (refuse for a read-only CD), `0x0220` VDiskRelease (unmap),
    `0x0410` ClientStatusMessage, heartbeat `0x0400` every 10 s while mapped.
