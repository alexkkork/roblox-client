#!/usr/bin/env python3
"""Select an accessible game for the explicitly requested alt-account test."""
import argparse
import json
from roblox_session import ROOT, RobloxSession, SessionError


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--place-id', type=int, help='Use the place explicitly selected by the user')
    args = parser.parse_args()
    if args.place_id is not None and not 1 <= args.place_id < 2**53:
        parser.error('Place ID must be a positive exact integer')
    session = RobloxSession()
    who = session.request("users.roblox.com", "/v1/users/authenticated")
    if who["http_status"] != 200 or not isinstance(who["data"], dict):
        raise SessionError("The alt account session is not authenticated")
    user_id = who["data"].get("id")
    if type(user_id) is not int:
        raise SessionError("The authenticated-user response was unexpected")
    selected = None
    if args.place_id is not None:
        selected = {'place_id': args.place_id, 'selection': 'user_selected_place'}
    else:
        games = session.request("games.roblox.com", "/v2/users/%d/games?accessFilter=Public&sortOrder=Asc&limit=10" % user_id)
        rows = games["data"].get("data", []) if isinstance(games["data"], dict) else []
        for game in rows:
            place = game.get("rootPlace") or {}
            if type(place.get("id")) is int:
                selected = {"place_id": place["id"], "universe_id": game.get("id"),
                            "selection": "alt_owned_public_place", "display_name": "Alt-owned public place"}
                break
    if selected is None:
        selected = {"place_id": 1818, "selection": "public_fallback_candidate"}
    details = session.request("games.roblox.com", "/v1/games/multiget-place-details?placeIds=%d" % selected["place_id"])
    data = details["data"]
    if details["http_status"] != 200 or not isinstance(data, list) or len(data) != 1:
        raise SessionError("Could not verify access to the candidate place")
    place = data[0]
    selected.update({"details_http_status": details["http_status"], "is_playable": place.get("isPlayable"),
                     "universe_id": place.get("universeId"), "universe_root_place_id": place.get("universeRootPlaceId")})
    if selected["selection"] in ('public_fallback_candidate', 'user_selected_place'):
        selected["display_name"] = place.get("name")
        selected["owned_by_alt"] = place.get("creatorId") == user_id
    if selected["is_playable"] is not True:
        selected["blocked_reason"] = place.get("reasonProhibited", "unknown")
    out = ROOT / "outputs/gamejoin"
    out.mkdir(parents=True, exist_ok=True)
    current = out / 'target-selection.json'
    if current.exists():
        previous = json.loads(current.read_text())
        if previous.get('place_id') != selected['place_id']:
            (out / ('target-selection-%s.json' % previous['place_id'])).write_text(json.dumps(previous, indent=2) + '\n')
    current.write_text(json.dumps(selected, indent=2) + "\n")
    print(json.dumps(selected, indent=2))
    return 0 if selected["is_playable"] else 1


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except SessionError as error:
        print(json.dumps({"error": str(error)}))
        raise SystemExit(1)
    except Exception as error:
        print(json.dumps({"error_type": type(error).__name__}))
        raise SystemExit(1)
