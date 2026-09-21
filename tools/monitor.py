#!/usr/bin/env python3
"""Reset the ESP32-S3 into its app via RTS and print serial output for N seconds."""
import os, sys, time, termios, fcntl, struct, select
port = sys.argv[1] if len(sys.argv) > 1 else "/dev/cu.usbmodem1101"
secs = float(sys.argv[2]) if len(sys.argv) > 2 else 12
reset = (sys.argv[3] if len(sys.argv) > 3 else "reset") == "reset"
fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
attrs = termios.tcgetattr(fd)
attrs[0] = 0; attrs[1] = 0; attrs[3] = 0
attrs[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
attrs[4] = attrs[5] = termios.B115200
termios.tcsetattr(fd, termios.TCSANOW, attrs)
def modem(bit, on):
    fcntl.ioctl(fd, termios.TIOCMBIS if on else termios.TIOCMBIC, struct.pack("I", bit))
if reset:
    modem(termios.TIOCM_DTR, False)   # DTR low: do not enter download mode
    modem(termios.TIOCM_RTS, True)    # RTS high: hold in reset
    time.sleep(0.2)
    modem(termios.TIOCM_RTS, False)   # release
end = time.time() + secs
while time.time() < end:
    r, _, _ = select.select([fd], [], [], 0.5)
    if r:
        try:
            data = os.read(fd, 65536)
        except BlockingIOError:
            continue
        sys.stdout.write(data.decode("utf-8", "replace")); sys.stdout.flush()
os.close(fd)
