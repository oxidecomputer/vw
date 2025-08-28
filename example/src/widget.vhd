library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;
use work.widget_pkg.all;

entity widget is
    port (
        clk : in std_logic;
        rst : in std_logic;
        enable : in std_logic;
        data_in : in std_logic_vector(7 downto 0);
        data_out : out std_logic_vector(7 downto 0);
        valid_out : out std_logic
    );
end entity widget;

architecture rtl of widget is
    signal counter : unsigned(WIDGET_WIDTH-1 downto 0) := (others => '0');
begin
    process(clk)
    begin
        if rising_edge(clk) then
            if rst = '1' then
                counter <= (others => '0');
                data_out <= DEFAULT_VALUE;
                valid_out <= '0';
            elsif enable = '1' then
                counter <= counter + 1;
                data_out <= increment_data(std_logic_vector(counter + unsigned(data_in)));
                valid_out <= '1';
            else
                valid_out <= '0';
            end if;
        end if;
    end process;
end architecture rtl;