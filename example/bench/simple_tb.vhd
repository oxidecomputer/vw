library ieee;
use ieee.std_logic_1164.all;

entity simple_tb is
end entity simple_tb;

architecture sim of simple_tb is
    signal test_signal : std_logic := '0';
begin
    process
    begin
        wait for 10 ns;
        test_signal <= '1';
        wait for 10 ns;
        test_signal <= '0';
        wait for 10 ns;
        
        report "Simple testbench completed!" severity note;
        std.env.finish;
        wait;
    end process;
end architecture sim;