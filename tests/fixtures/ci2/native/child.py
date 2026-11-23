import os
import subprocess
import sys
import time

mode = sys.argv[1]
identity = sys.argv[2]
if mode == "zero-fd":
    time.sleep(0.2)
    print("zero-fd-complete")
elif mode == "failed":
    sys.exit(7)
elif mode in ("fd-closed-live", "fd-closed-zombie", "term-resistant"):
    program = (
        ("import os,signal,time;signal.signal(signal.SIGTERM,signal.SIG_IGN);"
         "os.close(1);os.close(2);time.sleep(30)"
         if mode == "term-resistant" else
         "import os,time;os.close(1);os.close(2);time.sleep(30)")
        if mode != "fd-closed-zombie" else
        "import os;os.close(1);os.close(2)"
    )
    child = subprocess.Popen([sys.executable, "-c", program], stdin=subprocess.DEVNULL)
    with open(identity, "x", encoding="ascii") as record:
        record.write(str(child.pid))
    if mode == "term-resistant":
        import signal
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    # PID and PGID start identity become observable before leader exits.
    time.sleep(.2 if mode == "fd-closed-live" else 30) if mode == "term-resistant" else time.sleep(.2)
else:
    raise SystemExit(2)
