library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;

entity counter is
    port (
        clk : in std_logic;
        rst : in std_logic;
        enable : in std_logic;
        count_out : out unsigned(7 downto 0)
    );
end entity counter;

architecture rtl of counter is
    signal count : unsigned(7 downto 0) := (others => '0');
begin
    process(clk)
    begin
        if rising_edge(clk) then
            if rst = '1' then
                count <= (others => '0');
            elsif enable = '1' then
                count <= count + 1;
            end if;
        end if;
    end process;
    
    count_out <= count;
end architecture rtl;