contract Storage {
    uint256 public foo;

    constructor(uint256 x) {
        foo = x;
    }

    function set(uint256 x) external {
        foo = x;
    }

    function get() public returns (uint256) {
        return foo;
    }
}
