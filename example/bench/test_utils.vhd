library ieee;
use ieee.std_logic_1164.all;

package test_utils is
    procedure wait_cycles(signal clk: in std_logic; cycles: natural);
    constant TEST_TIMEOUT : time := 1 ms;
end package test_utils;

package body test_utils is
    procedure wait_cycles(signal clk: in std_logic; cycles: natural) is
    begin
        for i in 1 to cycles loop
            wait until rising_edge(clk);
        end loop;
    end procedure wait_cycles;
end package body test_utils;