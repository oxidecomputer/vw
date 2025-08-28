library ieee;
use ieee.std_logic_1164.all;
use ieee.numeric_std.all;

package widget_pkg is
    constant WIDGET_WIDTH : natural := 8;
    constant DEFAULT_VALUE : std_logic_vector(WIDGET_WIDTH-1 downto 0) := x"42";
    
    function increment_data(data : std_logic_vector(WIDGET_WIDTH-1 downto 0)) 
        return std_logic_vector;
end package widget_pkg;

package body widget_pkg is
    function increment_data(data : std_logic_vector(WIDGET_WIDTH-1 downto 0)) 
        return std_logic_vector is
    begin
        return std_logic_vector(unsigned(data) + 1);
    end function increment_data;
end package body widget_pkg;