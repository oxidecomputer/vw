library ieee;
use ieee.std_logic_1164.all;

entity broken_tb is
end entity broken_tb;

architecture sim of broken_tb is
    signal test_signal : std_logic := '0';
begin
    process
    begin
        wait for 10 ns;
        test_signal <= '1';
        wait for 10 ns;
        test_signal <= '0'; -- Fixed semicolon
        wait for 10 ns;
        
        report "This should not run!" severity note;
        std.env.finish;
        wait;
    end process;
end architecture sim;