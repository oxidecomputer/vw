library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;

entity counter_test_tb is
end entity counter_test_tb;

architecture sim of counter_test_tb is
    signal clk : std_logic := '0';
    signal rst : std_logic := '1';
    signal enable : std_logic := '0';
    signal count_out : unsigned(7 downto 0);
    
    constant clk_period : time := 10 ns;

begin
    -- Direct entity instantiation - this should be detected as dependency
    counter_inst: entity work.counter
        port map (
            clk => clk,
            rst => rst,
            enable => enable,
            count_out => count_out
        );

    -- Clock process
    clk_process: process
    begin
        clk <= '0';
        wait for clk_period/2;
        clk <= '1';
        wait for clk_period/2;
    end process;

    -- Test process
    test_process: process
    begin
        rst <= '1';
        wait for 20 ns;
        rst <= '0';
        enable <= '1';
        wait for 100 ns;
        enable <= '0';
        wait for 20 ns;
        
        report "Counter test completed!" severity note;
        std.env.finish;
        wait;
    end process;

end architecture sim;