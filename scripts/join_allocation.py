#!/usr/bin/env python3
"""Request a normal join allocation for the selected playable test place.

No game protocol packets are sent by this module. Reports contain structure and
status only; any connection credentials remain in the private secrets directory.
"""
from datetime import datetime, timezone
import json
import os
import stat
import time
import uuid
from roblox_session import ROOT, RobloxSession, SessionError


def shape(value, depth=0):
    if isinstance(value, dict):
        if depth >= 3:
            return {"type": "object", "keys": sorted(value)}
        return {str(k): shape(v, depth + 1) for k, v in value.items()}
    if isinstance(value, list):
        return {"type": "array", "length": len(value), "first_item": shape(value[0], depth + 1) if value else None}
    if isinstance(value, str):
        return {"type": "string", "length": len(value)}
    if value is None:
        return {"type": "null"}
    return {"type": type(value).__name__}


def write_private(path, value):
    from platform_files import open_nofollow
    flags = os.O_WRONLY | os.O_CREAT | os.O_TRUNC
    fd = open_nofollow(path, flags, 0o600)
    try:
        if os.name != "nt": os.fchmod(fd, 0o600)
        with os.fdopen(fd, "w") as output:
            fd = None
            json.dump(value, output)
            output.write("\n")
    finally:
        if fd is not None:
            os.close(fd)


def main(*, target=None):
    # A caller may hold a verified target for its own session while another
    # frontend changes the default selection. Never rewrite that shared choice.
    if target is None:
        target = json.loads((ROOT / "outputs/gamejoin/target-selection.json").read_text())
    elif isinstance(target, dict):
        target = target.copy()
    else:
        raise SessionError('Explicit allocation target must be an object')
    if target.get("is_playable") is not True or target.get("selection") not in ('alt_owned_public_place', 'user_selected_place', 'user_selected_private_server'):
        raise SessionError("This probe requires a verified playable place selected for this test")
    session = RobloxSession()
    payload = {"placeId": target["place_id"], "gameJoinAttemptId": str(uuid.uuid4()), "isTeleport": False}
    private = target['selection'] == 'user_selected_private_server'
    endpoint = '/v1/join-private-game' if private else '/v1/join-game'
    invite = None
    if private:
        invite = json.loads((ROOT / 'secrets/private-server-target.json').read_text())
        if invite.get('status') != 'Valid' or invite.get('placeId') != target['place_id']:
            raise SessionError('Private invite does not match the selected place')
        payload['linkCode'] = invite['linkCode']
    attempts = []
    for index in range(8):
        reply = session.request("gamejoin.roblox.com", endpoint, "POST", payload, csrf_retry=True)
        body = reply["data"]
        attempts.append({"http_status": reply["http_status"], "join_status": body.get("status") if isinstance(body, dict) else None})
        pending = (reply["http_status"] == 200 and not reply["challenge_present"]
                   and isinstance(body, dict) and body.get("status") in (0, 1)
                   and not body.get("joinScript") and not body.get("joinScriptUrl"))
        if not pending or index == 7:
            break
        time.sleep(3)
    report = {"checked_at": datetime.now(timezone.utc).isoformat(), "place_id": target["place_id"],
              "endpoint": "https://gamejoin.roblox.com" + endpoint, "method": "POST",
              "http_status": reply["http_status"], "challenge_present": reply["challenge_present"],
              "response_shape": shape(body), "attempts": attempts, "native_game_connection_established": False}
    if isinstance(body, dict):
        if type(body.get("status")) is int:
            report["join_status"] = body["status"]
        # Publish this known, non-personal error without copying arbitrary
        # response strings (which could contain credentials or account data).
        if body.get('message') == 'User privacy does not allow join.':
            report['user_privacy_blocks_join'] = True
            report['join_status_message'] = 'User privacy does not allow join.'
        errors = body.get("errors", [])
        if isinstance(errors, list):
            report["error_codes"] = [x.get("code") for x in errors if isinstance(x, dict)]
    ready = (reply["http_status"] == 200 and not reply["challenge_present"]
             and isinstance(body, dict) and body.get("status") == 2 and isinstance(body.get("joinScript"), dict))
    if ready:
        if private:
            join = body['joinScript']
            if (not join.get('PrivateServerID') or join.get('PrivateServerOwnerID') != invite['ownerUserId']
                    or join.get('PlaceId') != target['place_id']):
                raise SessionError('Issued allocation did not match the requested private server')
            report['private_server_allocation_verified'] = True
        saved = {"received_at": report["checked_at"], "place_id": target["place_id"], "body": body,
                 'selection': target['selection']}
        if private: saved['private_server_invite_id'] = invite['privateServerId']
        write_private(ROOT / "secrets/last-join.json", saved)
        report["private_reply_saved"] = True
    (ROOT / "outputs/gamejoin").mkdir(parents=True, exist_ok=True)
    (ROOT / "outputs/gamejoin/allocation-check.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if ready else 2


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SessionError as error:
        print(json.dumps({"error": str(error)}))
        raise SystemExit(1)
    except Exception as error:
        print(json.dumps({"error_type": type(error).__name__}))
        raise SystemExit(1)
