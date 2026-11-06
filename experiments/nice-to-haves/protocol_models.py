"""Deterministic, single-process teaching models. No I/O or durability guarantees."""

import json
from collections import deque


class ChatRoom:
    """One already-authenticated room; caller identities are supplied by the harness.

    Operation IDs deduplicate exact UTF-8 message bytes per caller until TTL.
    A retained operation replays its sequence even if the event left the replay ring.
    Expired IDs can execute again. Capacity rejects rather than evicting live IDs.
    """

    def __init__(self, ttl=10, operations=8, history=4):
        if any(type(x) is not int or x <= 0 for x in (ttl, operations, history)):
            raise ValueError("positive integer capacities and TTL required")
        self.ttl, self.capacity = ttl, operations
        self.events = deque(maxlen=history)
        self.operations = {}
        self.sequence = 0
        self.clock = 0

    def send(self, now, caller, operation, body):
        if type(now) is not int or now < self.clock:
            raise ValueError("nondecreasing integer clock required")
        if any(not isinstance(x, str) or not 0 < len(x.encode('utf-8')) <= limit
               for x, limit in ((caller, 128), (operation, 128), (body, 4096))):
            raise ValueError("bounded nonempty UTF-8 caller, operation and body required")
        self.clock = now
        self.operations = {k: v for k, v in self.operations.items() if v[0] > now}
        key = (caller, operation)
        if key in self.operations:
            _, original, sequence = self.operations[key]
            return {"status": "replay" if original == body else "conflict",
                    "sequence": sequence}
        if len(self.operations) >= self.capacity:
            return {"status": "full"}
        self.sequence += 1
        self.operations[key] = (now + self.ttl, body, self.sequence)
        self.events.append({"sequence": self.sequence, "caller": caller,
                            "operation": operation, "body": body})
        return {"status": "accepted", "sequence": self.sequence}

    def replay(self, cursor):
        if type(cursor) is not int or not 0 <= cursor <= self.sequence:
            raise ValueError("cursor must refer to this room's sequence")
        if self.events and cursor < self.events[0]['sequence'] - 1:
            return {"status": "gap", "latest": self.sequence}
        return {"status": "ok", "events": [dict(e) for e in self.events
                                             if e['sequence'] > cursor]}


class ChatCursor:
    """Advance only on contiguous delivery. A gap requires explicit resynchronization."""

    def __init__(self):
        self.sequence = 0

    def receive(self, sequence):
        if type(sequence) is not int or sequence < 1:
            raise ValueError("positive event sequence required")
        if sequence <= self.sequence:
            return "duplicate"
        if sequence != self.sequence + 1:
            return "gap"
        self.sequence = sequence
        return "display"


def demo():
    room, cursor = ChatRoom(), ChatCursor()
    # The first result is deliberately 'lost'; only the retry reaches the client.
    lost = room.send(0, 'alice', 'message-1', 'Hello 🌍')
    retry = room.send(1, 'alice', 'message-1', 'Hello 🌍')
    conflict = room.send(2, 'alice', 'message-1', 'Different')
    events = room.replay(0)['events']
    return {'basis': 'synthetic in-memory example', 'lost_response': lost,
            'retry': retry, 'conflict': conflict,
            'deliveries': [cursor.receive(e['sequence']) for e in events] +
                          [cursor.receive(events[0]['sequence'])],
            'cursor': cursor.sequence,
            'after_expiry': room.send(10, 'alice', 'message-1', 'Hello 🌍')}


if __name__ == '__main__':
    print(json.dumps(demo(), ensure_ascii=False, sort_keys=True, indent=2))
