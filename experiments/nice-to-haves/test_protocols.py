import unittest

from protocol_models import ChatCursor, ChatRoom, demo


class ProtocolTests(unittest.TestCase):
    def test_lost_reply_conflict_and_expiry(self):
        result = demo()
        self.assertEqual(result['retry'], {'status': 'replay', 'sequence': 1})
        self.assertEqual(result['conflict'], {'status': 'conflict', 'sequence': 1})
        self.assertEqual(result['deliveries'], ['display', 'duplicate'])
        self.assertEqual(result['after_expiry'], {'status': 'accepted', 'sequence': 2})

    def test_scope_exact_bytes_and_non_sliding_ttl(self):
        room = ChatRoom(ttl=10)
        self.assertEqual(room.send(0, 'alice', 'x', 'é')['sequence'], 1)
        self.assertEqual(room.send(1, 'bob', 'x', 'é')['sequence'], 2)
        self.assertEqual(room.send(8, 'alice', 'x', 'e\u0301')['status'], 'conflict')
        self.assertEqual(room.send(9, 'alice', 'x', 'é')['status'], 'replay')
        self.assertEqual(room.send(10, 'alice', 'x', 'é')['sequence'], 3)
        self.assertEqual(ChatRoom().send(0, 'alice', 'x', 'é')['sequence'], 1)

    def test_capacity_never_evicts_live_operation(self):
        room = ChatRoom(operations=1, history=1)
        room.send(0, 'a', 'x', 'one')
        self.assertEqual(room.send(1, 'a', 'y', 'two'), {'status': 'full'})
        self.assertEqual(room.send(1, 'a', 'x', 'one')['status'], 'replay')
        self.assertEqual(room.replay(0)['events'][0]['body'], 'one')
        self.assertEqual(room.send(10, 'a', 'y', 'two')['sequence'], 2)
        self.assertEqual(room.replay(0), {'status': 'gap', 'latest': 2})
        self.assertEqual(room.replay(1)['events'][0]['body'], 'two')
        self.assertEqual(room.replay(2), {'status': 'ok', 'events': []})

    def test_retained_operation_outlives_ring_and_copies_events(self):
        room = ChatRoom(history=1)
        room.send(0, 'a', 'x', 'one')
        room.send(0, 'a', 'y', 'two')
        self.assertEqual(room.send(1, 'a', 'x', 'one')['sequence'], 1)
        room.replay(1)['events'][0]['body'] = 'tampered'
        self.assertEqual(room.replay(1)['events'][0]['body'], 'two')

    def test_cursor_does_not_silently_skip_gap(self):
        cursor = ChatCursor()
        self.assertEqual(cursor.receive(2), 'gap')
        self.assertEqual(cursor.sequence, 0)
        self.assertEqual(cursor.receive(1), 'display')
        self.assertEqual(cursor.receive(1), 'duplicate')
        self.assertEqual(cursor.receive(3), 'gap')
        self.assertEqual(cursor.receive(2), 'display')

    def test_input_boundaries(self):
        room = ChatRoom()
        room.send(2, 'a', 'x', 'a' * 4096)
        for args in [(1, 'a', 'x', 'ok'), (True, 'a', 'x', 'ok'),
                     (2, '', 'x', 'ok'), (2, 'a', 'x', 'é' * 2049)]:
            with self.assertRaises(ValueError):
                room.send(*args)
        for cursor in [-1, 2, True]:
            with self.assertRaises(ValueError):
                room.replay(cursor)


if __name__ == '__main__':
    unittest.main()
