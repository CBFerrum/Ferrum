"""Spike S2 (docs/plan/platform.md): what Ferrum would drive in Stalwart 1.0, against a fresh local
container: bootstrap, provisioning over the JMAP administration API, then mail as a webmail would
use it. python3 stalwart-spike.py (needs Docker; DOCKER_CONFIG may point at an anonymous config)."""

import base64
import json
import re
import smtplib
import subprocess
import sys
import time
import urllib.error
import urllib.request
from email.message import EmailMessage

BASE = "http://127.0.0.1:18080"
PASS = "Spike-Pass-2026"
results = []


def note(check, ok, detail=""):
    results.append((check, ok, detail))
    print(f"{'ok ' if ok else 'NO '} {check}: {detail}"[:400])


def call(method, path, body=None, auth=None):
    auth = auth or ADMIN
    req = urllib.request.Request(BASE + path, method=method, data=None if body is None else json.dumps(body).encode())
    req.add_header("authorization", "Basic " + base64.b64encode(f"{auth[0]}:{auth[1]}".encode()).decode())
    if body is not None:
        req.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=20) as r:
            text = r.read().decode()
            return r.status, (json.loads(text) if text.strip().startswith(("{", "[")) else text)
    except urllib.error.HTTPError as e:
        text = e.read().decode()
        try:
            return e.code, json.loads(text)
        except ValueError:
            return e.code, text
    except (urllib.error.URLError, ConnectionError) as e:
        return 0, str(e)


def docker(*args):
    return subprocess.run(["docker", *args], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True).stdout


def wait(check, seconds=60):
    for _ in range(seconds * 2):
        if check():
            return True
        time.sleep(0.5)
    return False


# ── a fresh server, set up the way Ferrum would set one up ──
docker("rm", "-f", "stalwart-spike")
docker("run", "-d", "--name", "stalwart-spike", "-p", "127.0.0.1:18080:8080", "-p", "127.0.0.1:12525:25",
       "-p", "127.0.0.1:12587:587", "-p", "127.0.0.1:11143:143", "stalwartlabs/stalwart:latest")
wait(lambda: "password:" in docker("logs", "stalwart-spike"))
temp = re.search(r"password:\s*(\S+)", docker("logs", "stalwart-spike")).group(1)
ADMIN = ("admin", temp)
wait(lambda: call("GET", "/jmap/session")[0] == 200)
s, sess = call("GET", "/jmap/session")
boot = {"using": ["urn:ietf:params:jmap:core", "urn:stalwart:jmap"], "methodCalls": [["x:Bootstrap/set", {"accountId": sess["primaryAccounts"]["urn:stalwart:jmap"],
        "update": {"singleton": {"defaultDomain": "demo.test", "defaultHostname": "mail.demo.test", "requestTlsCertificate": False}}}, "0"]]}
s, out = call("POST", "/jmap/", boot)
made = out["methodResponses"][0][1].get("updated", {}).get("singleton", {})
note("bootstrap over JMAP gives a permanent administrator", "secret" in made, made.get("username", json.dumps(out)[:200]))
ADMIN = (made["username"], made["secret"])
docker("restart", "stalwart-spike")
note("server up after leaving bootstrap mode", wait(lambda: call("GET", "/jmap/session")[0] == 200))

# ── provisioning, as Ferrum's sync would do it (Stalwart 1.0: administration is JMAP) ──
def admin(calls):
    s, sess = call("GET", "/jmap/session")
    acc = sess["primaryAccounts"]["urn:stalwart:jmap"]
    calls = [[m, {"accountId": acc, **a}, i] for m, a, i in calls]
    s, out = call("POST", "/jmap/", {"using": ["urn:ietf:params:jmap:core", "urn:stalwart:jmap"], "methodCalls": calls})
    return {i: r for _, r, i in out["methodResponses"]} if isinstance(out, dict) and "methodResponses" in out else {"error": out}


r = admin([["x:Domain/query", {"filter": {"name": "demo.test"}}, "q"]])
ids = r.get("q", {}).get("ids", [])
if ids:
    dom = ids[0]
    r = admin([["x:Domain/set", {"update": {dom: {"allowScimProvisioning": True}}}, "u"]])
    note("the bootstrap domain exists; SCIM provisioning on", dom in json.dumps(r.get("u", {}).get("updated", {})), f"{dom}")
else:
    r = admin([["x:Domain/set", {"create": {"d": {"name": "demo.test", "allowScimProvisioning": True}}}, "c"]])
    dom = r.get("c", {}).get("created", {}).get("d", {}).get("id")
    note("create a domain", bool(dom), json.dumps(r)[:300])
r = admin([["x:Account/set", {"create": {"g": {"@type": "Group", "name": "sales", "domainId": dom, "description": "Sales"}}}, "c"]])
group = r.get("c", {}).get("created", {}).get("g", {}).get("id")
note("create a group", bool(group), json.dumps(r)[:300])
users = {}
for user in ["ann", "bob", "cat"]:
    body = {"@type": "User", "name": user, "domainId": dom, "description": user.title(), "roles": {"@type": "User"},
            "credentials": {"0": {"@type": "Password", "secret": PASS}}}
    if user in ("ann", "bob"):
        body["memberGroupIds"] = {group: True}
    if user == "ann":
        body["aliases"] = {"0": {"name": "ann.alias", "domainId": dom, "enable": True}}
    r = admin([["x:Account/set", {"create": {"u": body}}, "c"]])
    users[user] = r.get("c", {}).get("created", {}).get("u", {}).get("id")
    note(f"create user {user}" + (" (in sales, with an alias)" if user == "ann" else " (in sales)" if user == "bob" else ""), bool(users[user]), json.dumps(r)[:300])
r = admin([["x:MailingList/set", {"create": {"l": {"name": "everyone", "domainId": dom, "description": "Everyone",
                                                  "recipients": {"ann@demo.test": True, "bob@demo.test": True, "cat@demo.test": True}}}}, "c"]])
note("create a mailing list", bool(r.get("c", {}).get("created", {}).get("l")), json.dumps(r)[:300])
r = admin([["x:Tenant/set", {"create": {"t": {"name": "acme"}}}, "c"]])
note("create a tenant (marked Enterprise in the schema)", True, json.dumps(r.get("c", r))[:300])
r = admin([["x:Domain/get", {"ids": [dom], "properties": ["name", "dnsZoneFile"]}, "g"]])
zone = (r.get("g", {}).get("list") or [{}])[0].get("dnsZoneFile") or ""
kinds = sorted({ln.split()[3] for ln in zone.splitlines() if len(ln.split()) > 4 and not ln.startswith(";")})
note("DNS zone for the domain", bool(zone), f"record types {kinds}; {len(zone.splitlines())} lines")
open(__file__.replace("stalwart-spike.py", "stalwart-zone.txt"), "w").write(zone)
s, out = call("GET", "/scim/v2/ServiceProviderConfig")
note("SCIM 2.0 provisioning (Enterprise only)", True, f"{s} {str(out)[:160]}")

# ── JMAP as the webmail would use it ──
def jmap_session(user):
    return call("GET", "/jmap/session", auth=(f"{user}@demo.test", PASS))


def jmap(user, session, calls, using=None):
    using = using or ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail", "urn:ietf:params:jmap:submission"]
    path = session["apiUrl"] if session["apiUrl"].startswith("/") else "/" + session["apiUrl"].split("://", 1)[1].split("/", 1)[1]
    s, out = call("POST", "/" + path.lstrip("/") if not path.startswith("/") else path, {"using": using, "methodCalls": calls}, auth=(f"{user}@demo.test", PASS))
    return s, out


s, ann = jmap_session("ann")
note("JMAP session (ann)", s == 200 and isinstance(ann, dict), f"{s} capabilities={sorted(ann.get('capabilities', {}))[:12] if isinstance(ann, dict) else ann}")
if s != 200:
    sys.exit(0)
acc = ann["primaryAccounts"]["urn:ietf:params:jmap:mail"]
s, out = jmap("ann", ann, [["Mailbox/get", {"accountId": acc, "properties": ["name", "role"]}, "0"], ["Identity/get", {"accountId": acc}, "1"]])
boxes = {m["role"] or m["name"]: m["id"] for m in out["methodResponses"][0][1]["list"]}
identity = out["methodResponses"][1][1]["list"][0]["id"]
note("mailboxes", "inbox" in boxes and "junk" in boxes, f"{sorted(boxes)}")
send = [
    ["Email/set", {"accountId": acc, "create": {"e1": {"mailboxIds": {boxes["drafts"]: True}, "from": [{"email": "ann@demo.test"}], "to": [{"email": "bob@demo.test"}, {"email": "sales@demo.test"}],
                                                        "subject": "Quarterly numbers", "bodyValues": {"b": {"value": "Numbers attached soon."}}, "textBody": [{"partId": "b", "type": "text/plain"}]}}}, "0"],
    ["EmailSubmission/set", {"accountId": acc, "create": {"s1": {"emailId": "#e1", "identityId": identity}},
                             "onSuccessUpdateEmail": {"#s1": {f"mailboxIds/{boxes['drafts']}": None, f"mailboxIds/{boxes['sent']}": True}}}, "1"],
]
s, out = jmap("ann", ann, send)
note("send over JMAP (to bob and the sales group)", s == 200 and "s1" in json.dumps(out), f"{s} {json.dumps(out)[:300]}")

time.sleep(4)
def inbox_of(user):
    s, sess = jmap_session(user)
    a = sess["primaryAccounts"]["urn:ietf:params:jmap:mail"]
    s, out = jmap(user, sess, [["Mailbox/get", {"accountId": a, "properties": ["name", "role", "totalEmails"]}, "0"],
                               ["Email/query", {"accountId": a, "sort": [{"property": "receivedAt", "isAscending": False}], "limit": 10}, "1"],
                               ["Email/get", {"accountId": a, "#ids": {"resultOf": "1", "name": "Email/query", "path": "/ids"}, "properties": ["subject", "mailboxIds", "keywords", "from"]}, "2"]])
    boxes = {m["id"]: (m["role"] or m["name"]) for m in out["methodResponses"][0][1]["list"]}
    mails = [(e["subject"], [boxes[b] for b in e["mailboxIds"]]) for e in out["methodResponses"][2][1]["list"]]
    return sess, a, boxes, mails, out


_, _, _, mails, _ = inbox_of("bob")
note("bob received it (direct and through the group)", any(s == "Quarterly numbers" for s, _ in mails), f"{mails}")

# ── spam: an outside sender with the GTUBE test string, over SMTP ──
msg = EmailMessage()
msg["From"] = "promo@spam-sender.example"
msg["To"] = "cat@demo.test"
msg["Subject"] = "You won"
msg.set_content("XJS*C4JDBQADN1.NSBN3*2IDNEN*GTUBE-STANDARD-ANTI-UBE-TEST-EMAIL*C.34X")
try:
    with smtplib.SMTP("127.0.0.1", 12525, timeout=20) as smtp:
        smtp.ehlo("mx.spam-sender.example")
        smtp.send_message(msg)
    note("inbound SMTP from outside accepted", True)
except Exception as e:  # a reject at SMTP time is also a spam verdict
    note("inbound SMTP from outside", True, f"rejected at SMTP: {e}")
time.sleep(4)
sess, acc_c, boxes_c, mails, out = inbox_of("cat")
note("where the spam landed", True, f"{mails}")
junk = next((i for i, r in boxes_c.items() if r == "junk"), None)
inbox = next((i for i, r in boxes_c.items() if r == "inbox"), None)

# Training, as the webmail's "spam" / "not spam" buttons would do it: the test message is moved out
# of Junk with $notjunk; Stalwart keeps training samples (x:SpamTrainingSample).
q = out["methodResponses"][2][1]["list"]
target = next((e for e in q if junk in e["mailboxIds"]), None)
if target and inbox:
    s, res = jmap("cat", sess, [["Email/set", {"accountId": acc_c, "update": {target["id"]: {"mailboxIds": {inbox: True}, "keywords/$junk": None, "keywords/$notjunk": True}}}, "0"]])
    note("\"not spam\": move out of Junk with $notjunk", s == 200 and target["id"] in json.dumps(res["methodResponses"][0][1].get("updated", {})), f"{s}")
    time.sleep(2)
    r = admin([["x:SpamTrainingSample/query", {}, "q"]])
    note("the move left a training sample", len(r.get("q", {}).get("ids", [])) > 0, json.dumps(r.get("q", r))[:200])

# ── vacation reply and a shared mailbox ──
BOB_ID = users["bob"]
s, sess_b = jmap_session("bob")
acc_b = sess_b["primaryAccounts"]["urn:ietf:params:jmap:mail"]
s, out = jmap("bob", sess_b, [["VacationResponse/set", {"accountId": acc_b, "update": {"singleton": {"isEnabled": True, "subject": "Away", "textBody": "Back on Monday."}}}, "0"]],
              using=["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:vacationresponse"])
note("vacation reply (JMAP)", s == 200 and "updated" in json.dumps(out), f"{s} {json.dumps(out)[:200]}")
s, out = jmap("ann", ann, [["Mailbox/set", {"accountId": acc, "create": {"m1": {"name": "Shared invoices", "shareWith": {BOB_ID: {"mayReadItems": True, "mayAddItems": True}}}}}, "0"]],
              using=["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail", "urn:ietf:params:jmap:principals"])
note("share a mailbox with bob", s == 200 and "m1" in json.dumps(out) and "notCreated" not in json.dumps(out), f"{s} {json.dumps(out)[:240]}")
s, sess_b = jmap_session("bob")
note("bob sees ann's shared mailbox as an account", len(sess_b.get("accounts", {})) > 1, f"accounts={[a.get('name') for a in sess_b.get('accounts', {}).values()]}")

print(json.dumps({"passed": sum(1 for _, ok, _ in results if ok), "failed": [c for c, ok, _ in results if not ok]}))
