library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use work.test_utils.all;

entity widget_tb is
end entity widget_tb;

architecture sim of widget_tb is
    -- Component declaration
    component widget is
        port (
            clk : in std_logic;
            rst : in std_logic;
            enable : in std_logic;
            data_in : in std_logic_vector(7 downto 0);
            data_out : out std_logic_vector(7 downto 0);
            valid_out : out std_logic
        );
    end component widget;

    -- Signals
    signal clk : std_logic := '0';
    signal rst : std_logic := '1';
    signal enable : std_logic := '0';
    signal data_in : std_logic_vector(7 downto 0) := (others => '0');
    signal data_out : std_logic_vector(7 downto 0);
    signal valid_out : std_logic;

    -- Clock period
    constant clk_period : time := 10 ns;

begin
    -- Instantiate the Unit Under Test (UUT)
    uut: widget
        port map (
            clk => clk,
            rst => rst,
            enable => enable,
            data_in => data_in,
            data_out => data_out,
            valid_out => valid_out
        );

    -- Clock process
    clk_process: process
    begin
        clk <= '0';
        wait for clk_period/2;
        clk <= '1';
        wait for clk_period/2;
    end process;

    -- Stimulus process
    stim_process: process
    begin
        -- Reset
        rst <= '1';
        wait_cycles(clk, 2);
        rst <= '0';
        wait_cycles(clk, 1);

        -- Test case 1: Basic operation
        data_in <= x"05";
        enable <= '1';
        wait for 40 ns;

        -- Test case 2: Different input
        data_in <= x"10";
        wait for 40 ns;

        -- Test case 3: Disable
        enable <= '0';
        wait for 20 ns;

        -- Test case 4: Re-enable
        enable <= '1';
        data_in <= x"FF";
        wait for 40 ns;

        -- End simulation
        report "Testbench completed successfully!" severity note;
        std.env.finish;
        wait;
    end process;

end architecture sim;