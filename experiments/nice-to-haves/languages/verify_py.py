import sys

import wasmtime

sys.path.insert(0, sys.argv[1])
from host import Root

store = wasmtime.Store()
component = Root(store)
for name in ['', 'é é 🌍', 'line\nquote"', 'zero\0byte']:
    result = component.greet(store, name)
    if result != f'Hello, {name}!':
        raise AssertionError('component greeting mismatch')
print('4 Python component string round trips passed')
