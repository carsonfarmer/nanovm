import wit_world


class WitWorld(wit_world.WitWorld):
    def greet(self, name: str) -> str:
        return f"Hello, {name}!"
