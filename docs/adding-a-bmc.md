# Adding a BMC

Got a server whose BMC only offers a Java iKVM? This is how the two supported families
were brought up, and how to gather what's needed for a third. You'll need the vendor's
viewer working at least once (an old JRE in a VM is fine), `socat`, and ipmicow's
`debug` subcommands.

## 1. Is it a family ipmicow already speaks?

Many BMCs reuse one of the two stacks under a different badge:

- **ATEN iKVM** (Supermicro X9 era and some other ATEN-based boards): the viewer's main
  class is `tw.com.aten.ikvm.KVMMain` and it talks to **TCP 5900**.
- **Avocent APCP/DVC** (IBM IMM2, and reportedly Dell iDRAC6/7 and Avocent switches): the
  viewer is an `avct*Viewer.jar` with its main class under `com.avocent`, and its JNLP
  names a `kmport`/`vport` (**3900** on the IMM2). The first bytes on the wire are `APCP`.

Try `ipmicow <host>` first. If it detects a family but fails, the difference is usually
in the **token mint** (each vendor's web UI is different) or the **login message**, not
the video codec. Run the probes to see how far it gets:

```sh
ipmicow debug x9-probe <host>      # ATEN: mint, handshake, first video rects
ipmicow debug imm2-probe <host>    # Avocent: one APCP connect, no login needed
```

## 2. Capture the reference viewer

Download the viewer's JNLP from the web UI (the console/remote-control button) and look
at its `<argument>`s. It will name the host, the port(s) and a session token. Then run
the vendor viewer through a pair of local relays, so every byte it exchanges is logged
in plaintext. For an Avocent viewer:

```sh
# Relay two local ports to the BMC's KVM port. Separate ports keep the two
# connections' streams apart.
socat -v -x TCP-LISTEN:3901,reuseaddr,fork,bind=127.0.0.1 TCP:<bmc>:3900 2>kmport.log &
socat -v -x TCP-LISTEN:3902,reuseaddr,fork,bind=127.0.0.1 TCP:<bmc>:3900 2>vport.log &

# Start the viewer with the JNLP's arguments, but pointed at the relays.
java -cp avctIBMViewer.jar com.avocent.ibmc.kvm.Main \
  ip=127.0.0.1 kmport=3901 vport=3902 user=<token-from-jnlp> passwd= \
  <the JNLP's other arguments…>
```

Mint a fresh JNLP for each run, because tokens are single-use. For an ATEN viewer, do
the same with a relay on 5900 and pass `127.0.0.1` as the host argument.

Use the console briefly: let a screen draw, type a few keys, move the mouse, then quit.
Short captures are much easier to read.

## 3. Dissect it

```sh
ipmicow debug imm2-decap kmport.log      # -> imm-c2s.bin + imm-s2c.bin (+ imm-frame0.bin)
ipmicow debug imm2-walk imm-c2s.bin      # every APCP / BEEF / VID header the client sent
ipmicow debug imm2-walk imm-s2c.bin      # ... and what the BMC sent back
ipmicow debug imm2-decode imm-s2c.bin    # DVC-decode the video -> imm-frame0.bmp
```

If `imm2-decode` produces a recognisable screen, the video path already works and
only the session setup needs teaching. Compare the client's opening messages
(`imm2-walk imm-c2s.bin`) with what `src/imm2/protocol.rs` sends.

For the client's side of the story, decompile the viewer jar (`jadx` works well). See
the "reference client" section of [ibm-imm2.md](ibm-imm2.md) for how the Avocent jar is
laid out and how to defeat its string obfuscation.

## 4. Open an issue

Include:

- the board / server model, the BMC firmware version, and the viewer jar's name and version;
- the JNLP's argument **names** and port numbers (replace the token and your IPs);
- the `imm2-walk` output for the first few dozen messages each way, or the `x9-probe`
  output.

**Don't attach raw captures publicly.** They contain your session token, your BMC's
address and whatever was on the screen. Header walks are usually enough to start with.
